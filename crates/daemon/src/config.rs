//! 配置管理框架（计划 task 4）：TOML 强类型结构 + notify 热重载骨架。
//!
//! 设计：
//! - 三命名空间雏形：`[gateway]`（含授权 / 缓存 / 安全子节）、`[[outlets]]`（北向出口，
//!   每路独立声明编码 protobuf/json——计划北向编码决议）、`[[points]]`（点位平铺行，
//!   设备字段随行冗余，便于 CSV/XLSX 批量导入导出闭环）；
//! - 热重载：notify RecommendedWatcher 监听配置文件，**防抖窗口**内合并事件后整文件
//!   重解析；成功则原子替换共享快照并递增**版本号**，失败则保留旧快照并记 warn
//!   （错误路径不 panic，不中断采集）；
//! - 读侧通过 `Arc<ConfigShared>` 拿 `snapshot()`（`Arc<GatewayConfig>` 无锁读），
//!   `version()` 供调用方探测变更（调度器等按版本重建轮询计划）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::Deserialize;
// `watch` 方法来自 Watcher trait，必须在作用域内。
use notify::Watcher as _;

use crate::error::{DaemonError, DaemonResult};

/// 热重载防抖窗口：窗口内多次写入合并为一次重载。
pub const DEBOUNCE_WINDOW: Duration = Duration::from_millis(300);
/// 事件轮询间隔（防抖判定精度）。
const POLL_INTERVAL: Duration = Duration::from_millis(50);

fn default_gateway_id() -> String {
    "gw-unset".to_string()
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("data")
}

fn default_topic_prefix() -> String {
    "telemetry".to_string()
}

fn default_qos() -> u8 {
    1
}

fn default_frequency_ms() -> u64 {
    1000
}

fn default_heartbeat_secs() -> u64 {
    86_400
}

/// 北向出口编码（每路出口独立可选；计划决议：protobuf 默认 / json 可选）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutletEncoding {
    /// Protobuf 编码（默认）。
    #[default]
    Protobuf,
    /// JSON 编码（int64 → string 约定见 protocol-proto）。
    Json,
}

/// 授权配置雏形（task 22-24 填充语义）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LicensingSection {
    /// 云授权服务地址；None = 纯本地模式（C 档雏形）。
    pub cloud_url: Option<String>,
    /// 心跳间隔（秒；计划默认 24h）。
    pub heartbeat_interval_secs: u64,
}

impl Default for LicensingSection {
    fn default() -> Self {
        Self {
            cloud_url: None,
            heartbeat_interval_secs: default_heartbeat_secs(),
        }
    }
}

/// 缓存配置雏形（task 17-18 填充 SQLite 细节）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CacheSection {
    /// 缓存库文件路径（相对 data_dir）。
    pub sqlite_path: String,
    /// 缓存上限（MB；超出后按环形覆盖策略淘汰）。
    pub max_size_mb: u64,
    /// 保留天数（计划：离线缓存 ≥7 天）。
    pub retention_days: u32,
}

impl Default for CacheSection {
    fn default() -> Self {
        Self {
            sqlite_path: "cache.db".to_string(),
            max_size_mb: 512,
            retention_days: 7,
        }
    }
}

/// 安全配置雏形（task 25/31/34 填充 SQLCipher / TLS 细节）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SecuritySection {
    /// 北向/界面 TLS 证书路径。
    pub tls_cert_path: Option<String>,
    /// 北向/界面 TLS 私钥路径。
    pub tls_key_path: Option<String>,
    /// Web 管理界面账号鉴权开关（计划：强制账号密码 + HTTPS）。
    pub web_auth_enabled: bool,
}

impl Default for SecuritySection {
    fn default() -> Self {
        Self {
            tls_cert_path: None,
            tls_key_path: None,
            web_auth_enabled: true,
        }
    }
}

/// 管理面登录账号（task 57 全量接线：生产路凭证来源）。
#[derive(Debug, Clone, Deserialize)]
pub struct MgmtAuthUser {
    /// 用户名（登录主体；登录时精确匹配）。
    pub name: String,
    /// 角色字面量（`ops` / `lic_ops` / `risk` / `system`；由 mgmt 层
    /// `rbac::Role::from_str` 解析，未知角色该账号被跳过——fail-closed）。
    pub role: String,
    /// `SHA-256(password)` 的 hex 编码（服务端只存哈希，比对走恒时比较；
    /// 明文密码永不写入配置文件）。
    pub password_hash: String,
}

/// 管理面登录凭证段（**可选**；缺省时生产路无凭证，登录仅开发路可用，
/// 详见 `mgmt::auth_login` 的两路 fail-closed 说明）。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct MgmtAuthSection {
    /// 登录账号列表（空列表 = 无任何登录凭证 → 登录端点全拒）。
    #[serde(default)]
    pub users: Vec<MgmtAuthUser>,
}

/// `[gateway]` 命名空间。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct GatewaySection {
    /// 网关标识（平台侧登记）。
    pub gateway_id: String,
    /// 数据根目录（缓存 / 日志 / 授权状态落盘位置；容器内须挂持久卷）。
    pub data_dir: PathBuf,
    /// 授权配置。
    pub licensing: LicensingSection,
    /// 缓存配置。
    pub cache: CacheSection,
    /// 安全配置。
    pub security: SecuritySection,
}

impl Default for GatewaySection {
    fn default() -> Self {
        Self {
            gateway_id: default_gateway_id(),
            data_dir: default_data_dir(),
            licensing: LicensingSection::default(),
            cache: CacheSection::default(),
            security: SecuritySection::default(),
        }
    }
}

/// `[[outlets]]` 北向出口（每路独立：broker / topic / qos / tls / 编码）。
#[derive(Debug, Clone, Deserialize)]
pub struct OutletConfig {
    /// 出口名（唯一键，日志与诊断用）。
    pub name: String,
    /// MQTT broker 地址（如 `mqtts://broker.local:8883`）。
    pub broker: String,
    /// 主题前缀。
    #[serde(default = "default_topic_prefix")]
    pub topic_prefix: String,
    /// QoS 等级（0/1/2）。
    #[serde(default = "default_qos")]
    pub qos: u8,
    /// 是否启用 TLS。
    #[serde(default)]
    pub tls: bool,
    /// 该路出口的载荷编码（默认 protobuf）。
    #[serde(default)]
    pub encoding: OutletEncoding,
}

/// `[[points]]` 点位平铺行（设备级字段随行冗余，便于批量导入导出）。
#[derive(Debug, Clone, Deserialize)]
pub struct PointConfig {
    /// 南向设备标识。
    pub device_id: String,
    /// 点位标识（点位表主键）。
    pub point_id: String,
    /// 协议（modbus-tcp / modbus-rtu / opcua / s7 / mc / http / mqtt）。
    pub protocol: String,
    /// 设备接入地址（如 `192.168.1.10:502` 或串口号）。
    pub address: String,
    /// 采集频率（毫秒；计划指标 ≥100ms）。
    #[serde(default = "default_frequency_ms")]
    pub frequency_ms: u64,
}

/// 网关强类型配置根。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct GatewayConfig {
    /// `[gateway]` 命名空间。
    pub gateway: GatewaySection,
    /// `[[outlets]]` 北向出口列表。
    pub outlets: Vec<OutletConfig>,
    /// `[[points]]` 点位列表。
    pub points: Vec<PointConfig>,
    /// `[mgmt_auth]` 管理面登录凭证段（**可选**；缺省 = 生产路未配置凭证，
    /// 登录走 `mgmt::auth_login` 的开发路 / fail-closed 逻辑，既有字段语义不变）。
    #[serde(default)]
    pub mgmt_auth: Option<MgmtAuthSection>,
}

impl GatewayConfig {
    /// 从 TOML 文件加载。
    ///
    /// # Errors
    /// 读取失败映射 [`DaemonError::StorageError`]，解析失败映射
    /// [`DaemonError::ConfigError`]（错误路径不 panic）。
    pub fn load(path: impl AsRef<Path>) -> DaemonResult<Self> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)
            .map_err(|e| DaemonError::StorageError(format!("read {}: {e}", path.display())))?;
        let config: GatewayConfig = toml::from_str(&raw)?;
        Ok(config)
    }

    /// 从 TOML 字符串解析（测试与嵌入场景用）。
    ///
    /// # Errors
    /// 解析失败映射 [`DaemonError::ConfigError`]。
    pub fn parse(raw: &str) -> DaemonResult<Self> {
        Ok(toml::from_str(raw)?)
    }
}

/// 共享配置快照：读侧拿 `Arc<GatewayConfig>` 快照 + 版本号探测变更。
pub struct ConfigShared {
    config: RwLock<Arc<GatewayConfig>>,
    version: AtomicU64,
}

impl ConfigShared {
    /// 以初始配置创建（版本号 1）。
    pub fn new(config: GatewayConfig) -> Self {
        Self {
            config: RwLock::new(Arc::new(config)),
            version: AtomicU64::new(1),
        }
    }

    /// 当前配置快照（锁中毒时退回底层数据，读路径不 panic）。
    pub fn snapshot(&self) -> Arc<GatewayConfig> {
        self.config
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// 当前配置版本号（每次成功热重载 +1）。
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    /// 原子替换快照并递增版本，返回新版本号。
    fn store(&self, config: GatewayConfig) -> u64 {
        let mut guard = self
            .config
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = Arc::new(config);
        drop(guard);
        self.version.fetch_add(1, Ordering::AcqRel) + 1
    }
}

/// 配置文件热重载器（notify + 防抖 + 后台线程）。
///
/// 生命周期：`spawn` 后台线程消费文件事件 → 防抖窗口静默后整文件重解析 →
/// 成功则替换快照并 `info!` 记录新版本，失败则 `warn!` 保留旧快照；
/// `stop()` 请求退出并回收线程。
pub struct ConfigHotReloader {
    /// 持有 watcher 以维持监听；drop watcher 会停止接收事件（故不允许被读取消除）。
    #[allow(dead_code)]
    watcher: notify::RecommendedWatcher,
    stop: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ConfigHotReloader {
    /// 加载初始配置、启动监听与后台重载线程。
    ///
    /// # Errors
    /// 初始加载失败或 watcher 初始化失败时返回 [`DaemonError`]（此时未产生后台线程）。
    pub fn spawn(path: impl Into<PathBuf>) -> DaemonResult<(Self, Arc<ConfigShared>)> {
        let path: PathBuf = path.into();
        // 初始加载失败直接返回错误：调用方据此走安全模式（task 55），绝不静默空配置。
        let initial = GatewayConfig::load(&path)?;
        let shared = Arc::new(ConfigShared::new(initial));

        let watch_dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map_or_else(|| PathBuf::from("."), ToOwned::to_owned);
        let target = path.clone();
        let (event_tx, event_rx) = std::sync::mpsc::channel::<()>();

        let mut watcher =
            notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
                let Ok(event) = res else { return };
                // 只对目标文件本身的修改类事件触发（目录监听下过滤无关文件）。
                let hit = event.paths.iter().any(|p| p == &target);
                if hit && event.kind.is_modify() {
                    // 通道关闭（线程已退出）时静默丢弃。
                    let _ = event_tx.send(());
                }
            })
            .map_err(DaemonError::from)?;
        watcher
            .watch(&watch_dir, notify::RecursiveMode::NonRecursive)
            .map_err(DaemonError::from)?;

        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread_path = path.clone();
        let thread_stop = stop.clone();
        let thread_shared = shared.clone();
        let handle = std::thread::Builder::new()
            .name("config-hot-reload".to_string())
            .spawn(move || {
                reload_loop(thread_path, thread_shared, thread_stop, event_rx);
            })
            .map_err(|e| DaemonError::ConfigError(format!("spawn reload thread: {e}")))?;

        Ok((
            Self {
                watcher,
                stop,
                handle: Some(handle),
            },
            shared,
        ))
    }

    /// 请求后台线程退出并等待结束。
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for ConfigHotReloader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// 防抖重载主循环：静默窗口后整文件重解析。
fn reload_loop(
    path: PathBuf,
    shared: Arc<ConfigShared>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    event_rx: std::sync::mpsc::Receiver<()>,
) {
    let mut pending = false;
    let mut last_event: Option<Instant> = None;
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        match event_rx.recv_timeout(POLL_INTERVAL) {
            Ok(()) => {
                pending = true;
                last_event = Some(Instant::now());
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        }
        let Some(last) = last_event else { continue };
        if pending && last.elapsed() >= DEBOUNCE_WINDOW {
            pending = false;
            last_event = None;
            match GatewayConfig::load(&path) {
                Ok(config) => {
                    let version = shared.store(config);
                    tracing::info!(
                        version,
                        path = %path.display(),
                        "config hot-reloaded"
                    );
                }
                Err(err) => {
                    // 失败保留旧快照（版本号不变），绝不 panic、不中断采集。
                    tracing::warn!(
                        error = %err,
                        path = %path.display(),
                        "config hot-reload failed; keeping previous config"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE_TOML: &str = r#"
[gateway]
gateway_id = "gw-alpha"
data_dir = "data"

[gateway.licensing]
cloud_url = "https://licensing.example.com"
heartbeat_interval_secs = 86400

[gateway.cache]
sqlite_path = "cache.db"
max_size_mb = 512
retention_days = 7

[gateway.security]
web_auth_enabled = true

[[outlets]]
name = "north-1"
broker = "mqtts://broker.local:8883"
topic_prefix = "telemetry"
qos = 1
tls = true
encoding = "protobuf"

[[outlets]]
name = "north-2"
broker = "mqtt://backup.local:1883"
encoding = "json"

[[points]]
device_id = "dev-01"
point_id = "p_temp"
protocol = "modbus-tcp"
address = "192.168.1.10:502"
frequency_ms = 100
"#;

    /// QA: 示例 TOML（1 个 Modbus 设备 + MQTT 配置）解析成功，协议 Modbus、频率 100ms。
    #[test]
    fn parse_example_config() {
        let config = GatewayConfig::parse(EXAMPLE_TOML).expect("parse ok");
        assert_eq!(config.gateway.gateway_id, "gw-alpha");
        assert_eq!(
            config.gateway.licensing.cloud_url.as_deref(),
            Some("https://licensing.example.com")
        );
        assert_eq!(config.gateway.cache.retention_days, 7);
        assert!(config.gateway.security.web_auth_enabled);

        assert_eq!(config.outlets.len(), 2);
        let north1 = &config.outlets[0];
        assert_eq!(north1.qos, 1);
        assert!(north1.tls);
        assert_eq!(north1.encoding, OutletEncoding::Protobuf);
        // 每路出口独立编码：north-2 未写 encoding → 默认 protobuf？不——显式 json。
        assert_eq!(config.outlets[1].encoding, OutletEncoding::Json);

        let point = config
            .points
            .iter()
            .find(|p| p.device_id == "dev-01")
            .expect("modbus device row");
        assert_eq!(point.protocol, "modbus-tcp");
        assert_eq!(point.frequency_ms, 100);
    }

    /// 缺省值兜底：空 TOML 也能得到全默认配置（不 panic）。
    #[test]
    fn defaults_apply_for_empty_document() {
        let config = GatewayConfig::parse("").expect("empty toml defaults");
        assert_eq!(config.gateway.gateway_id, "gw-unset");
        assert_eq!(config.gateway.cache.retention_days, 7);
        assert!(config.outlets.is_empty());
        assert!(config.points.is_empty());
        // task 57：mgmt_auth 可选段缺省 = None（既有配置语义不变）。
        assert!(config.mgmt_auth.is_none(), "mgmt_auth must default to None");
    }

    /// QA: task 57 可选 `[mgmt_auth]` 段解析——users 数组逐行承接
    /// name / role / password_hash；无该段仍为 None（向后兼容）。
    #[test]
    fn mgmt_auth_section_parses_optional_users() {
        // users 缺失 → 空列表（serde(default)），段本身存在。
        let config = GatewayConfig::parse("[mgmt_auth]").expect("empty mgmt_auth section");
        let section = config.mgmt_auth.expect("section present");
        assert!(section.users.is_empty());

        let config = GatewayConfig::parse(
            r#"
[[mgmt_auth.users]]
name = "alice"
role = "system"
password_hash = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"

[[mgmt_auth.users]]
name = "bob"
role = "ops"
password_hash = "aa7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
"#,
        )
        .expect("mgmt_auth users parse");
        let section = config.mgmt_auth.expect("section present");
        assert_eq!(section.users.len(), 2);
        assert_eq!(section.users[0].name, "alice");
        assert_eq!(section.users[0].role, "system");
        assert_eq!(
            section.users[0].password_hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(section.users[1].role, "ops");
    }

    /// 非法 TOML 返回 ConfigError 而非 panic。
    #[test]
    fn invalid_toml_is_an_error() {
        let err = GatewayConfig::parse("[gateway\ngateway_id = ").expect_err("must fail");
        assert!(matches!(err, DaemonError::ConfigError(_)));
        assert_eq!(err.error_code(), crate::error::ERR_CONFIG);
    }

    /// QA: 从文件加载示例配置。
    #[test]
    fn load_from_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, EXAMPLE_TOML).expect("write");
        let config = GatewayConfig::load(&path).expect("load ok");
        assert_eq!(config.points[0].frequency_ms, 100);
    }

    /// QA: 热重载生效——修改文件后版本号递增、新频率可见。
    #[test]
    fn hot_reload_picks_up_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, EXAMPLE_TOML).expect("write");

        let (reloader, shared) = ConfigHotReloader::spawn(&path).expect("spawn");
        assert_eq!(shared.version(), 1, "initial load is version 1");

        let updated = EXAMPLE_TOML.replace("frequency_ms = 100", "frequency_ms = 250");
        std::fs::write(&path, updated).expect("rewrite");

        let deadline = Instant::now() + Duration::from_secs(10);
        while shared.version() < 2 {
            assert!(
                Instant::now() < deadline,
                "hot reload did not trigger within 10s (version={})",
                shared.version()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        let snapshot = shared.snapshot();
        assert_eq!(snapshot.points[0].frequency_ms, 250);
        drop(reloader);
    }

    /// QA 错误路径: 热重载遇到非法配置 → 保留旧快照、版本号不变、不 panic。
    #[test]
    fn hot_reload_failure_keeps_previous_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, EXAMPLE_TOML).expect("write");

        let (reloader, shared) = ConfigHotReloader::spawn(&path).expect("spawn");
        let version_before = shared.version();

        std::fs::write(&path, "[gateway\ngateway_id = ").expect("write broken toml");
        // 防抖 300ms + 余量：给失败重载足够时间发生。
        std::thread::sleep(Duration::from_millis(1200));

        assert_eq!(
            shared.version(),
            version_before,
            "failed reload must not bump version"
        );
        let snapshot = shared.snapshot();
        assert_eq!(
            snapshot.points[0].frequency_ms, 100,
            "previous config must survive"
        );
        drop(reloader);
    }
}
