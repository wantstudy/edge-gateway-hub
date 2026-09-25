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

use serde::{Deserialize, Serialize};
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

/// 试用天数默认（3 天，计划 task 23；与 `auth::client::TRIAL_DAYS` 口径一致）。
fn default_trial_days() -> u32 {
    3
}

/// 离线宽限天数默认（7 天，计划 task 22；与 `auth::client::GRACE_DAYS` 口径一致）。
fn default_grace_days() -> u32 {
    7
}

/// 北向出口编码（每路出口独立可选；计划决议：protobuf 默认 / json 可选）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OutletEncoding {
    /// Protobuf 编码（默认）。
    #[default]
    Protobuf,
    /// JSON 编码（int64 → string 约定见 protocol-proto）。
    Json,
}

/// 授权配置（计划 task 22-24：激活码 / 试用 / 宽限 + 云服务端点）。
///
/// ⚠️ `activation_code` 是**敏感凭据**：
/// - 本结构手写 [`std::fmt::Debug`]——激活码输出一律 `<redacted>`（**永不进日志**）；
/// - `activation_code` 标注 `serde(skip_serializing)`——配置导出 / API 回显**绝不携带**；
/// - 仓库与示例配置文件只允许**占位符**，真实激活码经环境变量 `IOTDAQ_ACTIVATION_CODE`
///   或管理 UI 注入（装配方注入 [`crate::license::LicenseRuntimeConfig::activation_code`]）。
#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct LicensingSection {
    /// 云授权服务地址；None = 纯本地模式（C 档雏形）。
    pub cloud_url: Option<String>,
    /// 心跳间隔（秒；计划默认 24h）。
    pub heartbeat_interval_secs: u64,
    /// 激活码（**敏感**；见结构体文档的脱敏纪律）。`None` = 未配置云端激活。
    #[serde(skip_serializing)]
    pub activation_code: Option<String>,
    /// 试用天数（默认 3，与 `auth::client::TRIAL_DAYS` 口径一致；计划 task 23）。
    pub trial_days: u32,
    /// 离线宽限天数（默认 7，与 `auth::client::GRACE_DAYS` 口径一致；计划 task 22）。
    pub grace_days: u32,
}

impl Default for LicensingSection {
    fn default() -> Self {
        Self {
            cloud_url: None,
            heartbeat_interval_secs: default_heartbeat_secs(),
            activation_code: None,
            trial_days: default_trial_days(),
            grace_days: default_grace_days(),
        }
    }
}

/// 手写 [`std::fmt::Debug`]：**激活码脱敏**（只暴露是否已配置，绝不输出原文）。
impl std::fmt::Debug for LicensingSection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LicensingSection")
            .field("cloud_url", &self.cloud_url)
            .field("heartbeat_interval_secs", &self.heartbeat_interval_secs)
            .field(
                "activation_code",
                &self.activation_code.as_ref().map(|_| "<redacted>"),
            )
            .field("trial_days", &self.trial_days)
            .field("grace_days", &self.grace_days)
            .finish()
    }
}

/// 缓存配置雏形（task 17-18 填充 SQLite 细节）。
#[derive(Debug, Clone, Deserialize, Serialize)]
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
#[derive(Debug, Clone, Deserialize, Serialize)]
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
#[derive(Debug, Clone, Deserialize, Serialize)]
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
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct MgmtAuthSection {
    /// 登录账号列表（空列表 = 无任何登录凭证 → 登录端点全拒）。
    #[serde(default)]
    pub users: Vec<MgmtAuthUser>,
}

/// `[gateway]` 命名空间。
#[derive(Debug, Clone, Deserialize, Serialize)]
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
#[derive(Debug, Clone, Deserialize, Serialize)]
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
    ///
    /// 与 `broker` 的 scheme 必须**一致**：`mqtts://` ⇔ `tls = true`；
    /// `mqtt://` ⇔ `tls = false`。冲突（如 `tls = true` 配 `mqtt://`）在
    /// [`crate::north::runtime::endpoint_from_outlet`] 处报错，**绝不猜测意图**。
    #[serde(default)]
    pub tls: bool,
    /// 服务端 CA 证书 PEM 路径（task 25；TLS 出口**可选**——缺省用**操作系统根证书库**
    /// 校验 broker 证书；2026-09-25 用户决策「证书配置可选」。仍无跳过校验路径）。
    ///
    /// `None` = 用操作系统根证书库（TLS 未启用时忽略）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_cert_path: Option<String>,
    /// 客户端证书 PEM 路径（task 25；mTLS，与 `client_key_path` **成对**）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_cert_path: Option<String>,
    /// 客户端私钥 PEM 路径（task 25；mTLS，与 `client_cert_path` **成对**）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_key_path: Option<String>,
    /// SNI / 服务端名校验覆盖（task 25；缺省用 `broker` 的 host）。
    ///
    /// 用途：broker 证书 SAN 与连接地址不同（如经 DNS 别名/负载均衡接入）时，
    /// 显式声明应校验的服务端名——**不是**「跳过校验」开关。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
    /// ALPN 协议列表（task 25；如 `["mqtt"]`，空 = 不协商）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alpn: Vec<String>,
    /// 该路出口的载荷编码（默认 protobuf）。
    #[serde(default)]
    pub encoding: OutletEncoding,
}

/// `[[points]]` 点位平铺行（设备级字段随行冗余，便于批量导入导出）。
#[derive(Debug, Clone, Deserialize, Serialize)]
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

/// 设备登记行（可选 `[[devices]]` 段；管理面写接口的设备事实源）。
///
/// 设计取舍：设备列表 = 本段 ∪ `points.device_id` 去重聚合。**点位行仍是采集
/// 唯一事实源**——设备的协议 / 频率始终优先按点位行推导；本段只承载「无点位
/// 设备的登记」与「显示名 / 启停覆盖」，旧配置无此段时 serde default 为空列表，
/// 完全向后兼容。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeviceConfig {
    /// 设备标识（唯一键；与点位行的 `device_id` 同一命名空间）。
    pub device_id: String,
    /// 显示名（`None` = 展示层回退 `device_id`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 启停（缺省 `true`）。
    #[serde(default = "default_device_enabled")]
    pub enabled: bool,
    /// 设备默认协议（可选；仅当该设备无点位行时在设备摘要中展示）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
}

fn default_device_enabled() -> bool {
    true
}

/// 网关强类型配置根。
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct GatewayConfig {
    /// `[gateway]` 命名空间。
    pub gateway: GatewaySection,
    /// `[[outlets]]` 北向出口列表。
    pub outlets: Vec<OutletConfig>,
    /// `[[points]]` 点位列表。
    pub points: Vec<PointConfig>,
    /// `[[devices]]` 设备登记段（**可选**，向后兼容：缺省空列表；见
    /// [`DeviceConfig`] 的取舍说明。序列化时空列表省略，避免旧版本 daemon
    /// 拒读新字段）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<DeviceConfig>,
    /// `[mgmt_auth]` 管理面登录凭证段（**可选**；缺省 = 生产路未配置凭证，
    /// 登录走 `mgmt::auth_login` 的开发路 / fail-closed 逻辑，既有字段语义不变）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
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

    /// 保存配置到 TOML 文件（管理面写路径）：
    /// toml 序列化 → `backup_before_rewrite` 写前原子备份 → 临时文件 + fsync +
    /// 同目录 rename 原子落盘。任一步失败即中止，原文件保持写前状态（或可从
    /// `.bak-<unix秒>` 备份恢复）。
    ///
    /// # Errors
    /// - 序列化失败 → [`DaemonError::ConfigError`]；
    /// - 备份 / 写盘失败 → [`DaemonError::StorageError`]（临时半成品一律清理）。
    pub fn save(&self, path: impl AsRef<Path>) -> DaemonResult<()> {
        let path = path.as_ref();
        let raw = toml::to_string_pretty(self)
            .map_err(|e| DaemonError::ConfigError(format!("serialize config: {e}")))?;
        let backup = crate::migrations::backup_before_rewrite(path)?;
        tracing::info!(
            target: "daemon::config",
            path = %path.display(),
            backup = %backup.display(),
            "config: backup created before rewrite"
        );
        atomic_write(path, raw.as_bytes())?;
        Ok(())
    }
}

/// 原子写文件：同目录临时文件 + fsync + rename（与 `backup_before_rewrite`
/// 同一原子性口径；失败清理半成品临时文件，绝不 panic）。
fn atomic_write(path: &Path, data: &[u8]) -> DaemonResult<()> {
    use std::io::Write as IoWrite;
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config")
        .to_string();
    let tmp = dir.join(format!(".{file_name}.tmp-{}", std::process::id()));
    let write_result = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(DaemonError::StorageError(format!(
            "atomic write {}: {e}",
            tmp.display()
        )));
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(DaemonError::StorageError(format!(
            "atomic write rename {} -> {}: {e}",
            tmp.display(),
            path.display()
        )));
    }
    Ok(())
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

    /// 原子替换快照并递增版本（管理面写路径专用）：`GatewayConfig::save`
    /// 落盘成功后调用，读侧**即时**可见新配置，无需等待 notify 防抖窗口。
    /// 随后 notify watch 对磁盘的重读为同内容幂等重放（版本再 +1，无害）。
    pub fn replace(&self, config: GatewayConfig) -> u64 {
        self.store(config)
    }
}

/// 热重载准入闸门（授权配额门控的注入点）：对**解析成功**的候选配置做终检，
/// `Err(reason)` = 拒绝本次重载（保留旧快照、版本号不变、`warn!` 记录原因）。
///
/// bootstrap 用 `LicenseRuntime::enforce_free_limits` 充当本闸门——免费版（Degraded）
/// 期间，设备数 / 协议 / 采集间隔超限的热重载一律拒绝（fail-closed，可解释）。
pub type ReloadGate = Arc<dyn Fn(&GatewayConfig) -> Result<(), String> + Send + Sync>;

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
    /// 加载初始配置、启动监听与后台重载线程（无准入闸门）。
    ///
    /// # Errors
    /// 初始加载失败或 watcher 初始化失败时返回 [`DaemonError`]（此时未产生后台线程）。
    pub fn spawn(path: impl Into<PathBuf>) -> DaemonResult<(Self, Arc<ConfigShared>)> {
        Self::spawn_with_gate(path, None)
    }

    /// 加载初始配置、启动监听与后台重载线程，并注入热重载准入闸门。
    ///
    /// 每次文件重载解析成功后先过闸门（[`ReloadGate`]）；被拒则保留旧快照
    /// （版本号不变）并 `warn!` 拒绝原因——免费版配额在运行期热加载同样 fail-closed。
    ///
    /// # Errors
    /// 初始加载失败或 watcher 初始化失败时返回 [`DaemonError`]（此时未产生后台线程）。
    pub fn spawn_with_gate(
        path: impl Into<PathBuf>,
        gate: Option<ReloadGate>,
    ) -> DaemonResult<(Self, Arc<ConfigShared>)> {
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
                reload_loop(thread_path, thread_shared, thread_stop, event_rx, gate);
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

/// 防抖重载主循环：静默窗口后整文件重解析；解析成功先过准入闸门再替换快照。
fn reload_loop(
    path: PathBuf,
    shared: Arc<ConfigShared>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    event_rx: std::sync::mpsc::Receiver<()>,
    gate: Option<ReloadGate>,
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
                    // 准入闸门（授权配额，fail-closed）：被拒则保留旧快照，
                    // 版本号不变，原因可解释（含恢复路径），绝不 panic。
                    if let Some(gate) = &gate {
                        if let Err(reason) = gate(&config) {
                            tracing::warn!(
                                reason = %reason,
                                path = %path.display(),
                                "config hot-reload rejected by license quota gate; \
                                 keeping previous config"
                            );
                            continue;
                        }
                    }
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
# ⚠️ 激活码是敏感凭据：此处只允许占位符；真实值经 env IOTDAQ_ACTIVATION_CODE 注入。
activation_code = "IOTDAQ-0000-0000-0000-0000"
trial_days = 3
grace_days = 7

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
        // task 19 尾巴：授权配置字段补齐（激活码 / 试用 / 宽限）。
        assert_eq!(
            config.gateway.licensing.activation_code.as_deref(),
            Some("IOTDAQ-0000-0000-0000-0000"),
            "activation_code must be configurable (placeholder in examples)"
        );
        assert_eq!(config.gateway.licensing.trial_days, 3);
        assert_eq!(config.gateway.licensing.grace_days, 7);
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
        // task 19 尾巴：授权配置新字段缺省（向后兼容：老配置文件不含这些键也能解析）。
        let licensing = &config.gateway.licensing;
        assert_eq!(licensing.activation_code, None);
        assert_eq!(
            licensing.trial_days, 3,
            "trial_days default = 3 (TRIAL_DAYS)"
        );
        assert_eq!(
            licensing.grace_days, 7,
            "grace_days default = 7 (GRACE_DAYS)"
        );
    }

    /// 激活码脱敏纪律（task 19 尾巴）：
    /// 1. `Debug` 输出只含 `<redacted>`，**绝不**含激活码原文（防日志泄露）；
    /// 2. `Serialize` 输出（配置导出 / API 回显）**不含** `activation_code` 键。
    #[test]
    fn activation_code_is_redacted_in_debug_and_serialize() {
        let section = LicensingSection {
            activation_code: Some("IOTDAQ-SECRETCODE-DO-NOT-LOG".to_string()),
            ..LicensingSection::default()
        };
        let dbg = format!("{section:?}");
        assert!(dbg.contains("<redacted>"), "Debug must redact: {dbg}");
        assert!(
            !dbg.contains("IOTDAQ-SECRETCODE"),
            "Debug leaked activation code: {dbg}"
        );

        let json = serde_json::to_string(&section).expect("serialize licensing section");
        assert!(
            !json.contains("activation_code") && !json.contains("IOTDAQ-SECRETCODE"),
            "Serialize leaked activation code: {json}"
        );
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

    /// QA: 热重载准入闸门（授权配额，fail-closed）—— 闸门拒绝期间版本号不变、
    /// 旧快照保留；闸门放行后（同一文件再触发一次事件）重载成功、版本号递增。
    #[test]
    fn hot_reload_gate_rejects_then_accepts_on_recovery() {
        use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, EXAMPLE_TOML).expect("seed");

        // 模拟「免费版（Degraded）→ 激活恢复」的闸门状态翻转。
        let degraded = Arc::new(AtomicBool::new(true));
        let gate: ReloadGate = {
            let degraded = Arc::clone(&degraded);
            Arc::new(move |_config| {
                if degraded.load(AtomicOrdering::SeqCst) {
                    Err(
                        "free-edition quota exceeded (license degraded): devices: 9 > 8 — \
                         activate a license to lift the limits"
                            .to_string(),
                    )
                } else {
                    Ok(())
                }
            })
        };

        let (reloader, shared) =
            ConfigHotReloader::spawn_with_gate(&path, Some(gate)).expect("spawn with gate");
        let version_before = shared.version();

        // 闸门拒绝：合法 TOML 但被闸门否决 → 版本号不变、旧快照保留。
        std::fs::write(
            &path,
            EXAMPLE_TOML.replace("frequency_ms = 100", "frequency_ms = 250"),
        )
        .expect("write candidate");
        std::thread::sleep(Duration::from_millis(1200));
        assert_eq!(
            shared.version(),
            version_before,
            "gate-rejected reload must not bump version"
        );
        assert_eq!(
            shared.snapshot().points[0].frequency_ms,
            100,
            "previous config must survive gate rejection"
        );

        // 授权恢复（Degraded → Licensed）后再次触发文件事件 → 同一候选配置过闸。
        degraded.store(false, AtomicOrdering::SeqCst);
        std::fs::write(
            &path,
            EXAMPLE_TOML.replace("frequency_ms = 100", "frequency_ms = 250"),
        )
        .expect("rewrite candidate");
        let deadline = Instant::now() + Duration::from_secs(10);
        while shared.version() == version_before && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            shared.version() > version_before,
            "gate-accepted reload must bump version"
        );
        assert_eq!(
            shared.snapshot().points[0].frequency_ms,
            250,
            "new config must be visible after gate acceptance"
        );
        drop(reloader);
    }

    /// QA: 管理面写路径 `save()`——落盘内容可往返解析、写前备份文件产生且
    /// 内容为写前快照、临时半成品不残留。
    #[test]
    fn save_roundtrips_and_creates_backup_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, EXAMPLE_TOML).expect("seed");
        let original_raw = EXAMPLE_TOML.to_string();

        let mut config = GatewayConfig::load(&path).expect("load");
        // 模拟管理面写操作：追加设备登记。
        config.devices.push(DeviceConfig {
            device_id: "dev-02".to_string(),
            name: Some("二号设备".to_string()),
            enabled: false,
            protocol: Some("s7".to_string()),
        });
        config.save(&path).expect("save");

        // 落盘内容可重新解析且语义一致。
        let reloaded = GatewayConfig::load(&path).expect("reload");
        assert_eq!(reloaded.devices.len(), 1);
        assert_eq!(reloaded.devices[0].device_id, "dev-02");
        assert!(!reloaded.devices[0].enabled);
        assert_eq!(reloaded.devices[0].protocol.as_deref(), Some("s7"));
        assert_eq!(reloaded.points[0].frequency_ms, 100);
        assert_eq!(reloaded.gateway.gateway_id, "gw-alpha");

        // 写前备份存在且逐字节为写前快照。
        let backups: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("config.toml.bak-")
            })
            .collect();
        assert!(!backups.is_empty(), "backup file must be created");
        let backup_raw = std::fs::read_to_string(backups[0].path()).expect("read backup");
        assert_eq!(
            backup_raw, original_raw,
            "backup must be the pre-write snapshot"
        );

        // 无 .tmp 半成品残留。
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "no tmp leftovers: {leftovers:?}");
    }

    /// QA: `[[devices]]` 可选段向后兼容——旧配置（无 devices 段）解析为空列表；
    /// 空列表序列化时省略该键（旧版本 daemon 仍可读新写出的文件）。
    #[test]
    fn devices_section_is_backward_compatible() {
        // 旧配置无 devices 段 → serde default 空列表。
        let config = GatewayConfig::parse(EXAMPLE_TOML).expect("parse legacy config");
        assert!(
            config.devices.is_empty(),
            "legacy config without [[devices]] must default to empty"
        );

        // 空列表序列化时省略 devices 键。
        let raw = toml::to_string_pretty(&config).expect("serialize");
        assert!(
            !raw.contains("devices"),
            "empty devices must be skipped when serializing: {raw}"
        );

        // 带 devices 段的配置序列化 → `[[devices]]` 数组表。
        let with_devices =
            GatewayConfig::parse("[[devices]]\ndevice_id = \"dev-x\"\nenabled = true\n")
                .expect("parse devices");
        let raw = toml::to_string_pretty(&with_devices).expect("serialize");
        assert!(
            raw.contains("[[devices]]"),
            "devices section emitted: {raw}"
        );
        assert!(raw.contains("device_id = \"dev-x\""));
        // mgmt_auth None 不产生空表垃圾。
        assert!(!raw.contains("mgmt_auth"));
    }

    /// task 25：`[[outlets]]` 的 TLS / mTLS 字段可解析（CA / 客户端证书 / 私钥 /
    /// SNI / ALPN）。
    #[test]
    fn config_outlet_tls_fields_parse() {
        let raw = r#"
[[outlets]]
name = "tls-1"
broker = "mqtts://broker.local:8883"
tls = true
ca_cert_path = "/certs/ca.crt"
client_cert_path = "/certs/client.crt"
client_key_path = "/certs/client.key"
server_name = "broker.local"
alpn = ["mqtt"]
"#;
        let config = GatewayConfig::parse(raw).expect("parse tls outlet");
        assert_eq!(config.outlets.len(), 1);
        let outlet = &config.outlets[0];
        assert!(outlet.tls);
        assert_eq!(outlet.ca_cert_path.as_deref(), Some("/certs/ca.crt"));
        assert_eq!(
            outlet.client_cert_path.as_deref(),
            Some("/certs/client.crt")
        );
        assert_eq!(outlet.client_key_path.as_deref(), Some("/certs/client.key"));
        assert_eq!(outlet.server_name.as_deref(), Some("broker.local"));
        assert_eq!(outlet.alpn, vec!["mqtt".to_string()]);
    }

    /// task 25：无 TLS 字段的旧 `[[outlets]]` 仍可解析（向后兼容），缺省为
    /// `None` / 空 ALPN；且未设置字段在序列化时省略（旧 daemon 仍可读新文件）。
    #[test]
    fn config_outlet_tls_fields_backward_compatible() {
        let config =
            GatewayConfig::parse("[[outlets]]\nname = \"legacy\"\nbroker = \"mqtt://h:1883\"\n")
                .expect("parse legacy outlet");
        let outlet = &config.outlets[0];
        assert!(outlet.ca_cert_path.is_none());
        assert!(outlet.client_cert_path.is_none());
        assert!(outlet.client_key_path.is_none());
        assert!(outlet.server_name.is_none());
        assert!(outlet.alpn.is_empty());

        let raw = toml::to_string_pretty(&config).expect("serialize");
        for absent in [
            "ca_cert_path",
            "client_cert_path",
            "client_key_path",
            "server_name",
            "alpn",
        ] {
            assert!(
                !raw.contains(absent),
                "unset `{absent}` must be omitted when serializing: {raw}"
            );
        }
    }
}
