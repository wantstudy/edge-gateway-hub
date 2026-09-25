//! task 51 — daemon 进程装配与生命周期（bootstrap / 优雅停机 / 看门狗）。
//!
//! ## 职责边界
//! - **做**：[`DaemonShared`] 生命周期共享态（state watch / Utc 纳秒心跳 / 配置快照）；
//!   [`BootstrapBuilder`] 链式装配（配置热重载 → 调度器 → 北向启动钩子 → Running）；
//!   优雅停机（调度器停 → 离线队列 flush → 北向停 → Stopped，总宽限期 30s）；
//!   看门狗（基于 `last_heartbeat` 陈旧度判定 + 单次重启钩子）。
//!   **集成波次接线**：启动早期完整性自检（task 50 `verify_self_integrity`，
//!   fail-safe 受限模式继续，debug 构建跳过）；OTA 启动判定钩子（task 35
//!   `boot_commit_or_rollback`，回滚显式 [WARN]）。SQLite 分库（telemetry/queue）
//!   的迁移框架接线在各库模块内完成（task 55 `run_migrations`）。
//!   **北向运行期接线（task 19 + task 54 最后一跳）**：注入 [`NorthRuntimeConfig`]
//!   后，Running 前按 `[[outlets]]` 构造 [`crate::north::runtime::NorthRuntime`]
//!   （每个出口一个 `MqttClient` + `NorthOutlet`：有界发送队列 / PUBACK 门控确认 /
//!   补发幂等 / 审计取走上报）并起驱动任务；运行期句柄挂在 [`DaemonShared`]
//!   （[`DaemonShared::north_runtime`]）供采集侧投递与 mgmt 观测；停机步骤 3 中止
//!   全部驱动任务。
//! - **不做**：MQTT 协议本身与出口选路（= `north` 域）；`OfflineQueue` 实例的
//!   **创建**由装配方完成（见 [`NorthRuntimeConfig::queue`]，bootstrap 只消费）。
//!
//! ## 实现红线
//! - 仅用 workspace 已锁定依赖；错误收敛为 [`DaemonError`]。
//! - 优雅停机顺序固定：调度器 → flush → north → Stopped；超宽限期 `tracing::error!`
//!   后中止（state 保持在 `Stopping`，**不谎报 `Stopped`**，并以 `shutdown_timed_out`
//!   标志位供诊断 / 测试观测）。
//!
//! ## V1 已知限制（随代码注释）
//! 1. 看门狗基于 `last_heartbeat` 陈旧度判定（`scheduler.rs` 的 `GroupStats` 只有
//!    polls/samples/errors 计数，无心跳字段——已核对源码）；心跳由采集链路经
//!    [`DaemonShared::touch_heartbeat`] 上报，从未上报（= 0）时不做陈旧判定。
//! 2. 看门狗重启 = **单次**调用 `watchdog_restart` 钩子（默认 no-op warn）：
//!    V1 无法重建已被消费的 `RunningScheduler`，仅记录 + 钩子回调；
//!    组件级真正的重启留待装配闭环任务。
//! 3. 离线队列 flush 为钩子（默认 no-op + TODO）：bootstrap 只**消费**装配方注入的
//!    `OfflineQueue`，不负责其创建（构造需要 db 路径与 gateway_id 装配决策）。
//! 4. `run()` 支持外部注入 [`DaemonShared`]（[`BootstrapBuilder::with_shared`]）：
//!    管理 API（mgmt）与北向都需要在运行期持有同一句柄，故不能只在 run 结束时返回。
//! 5. 北向出口的 **TLS/mTLS 在 V1 不可接线**（`[[outlets]]` 无证书字段且不得新增
//!    配置段）：此类出口被显式拒绝并计入
//!    [`crate::north::runtime::NorthRuntimeStats::skipped`]，**不静默退化为明文**。

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;
use tracing::{error, info, warn};

use crate::config::{ConfigHotReloader, ConfigShared, GatewayConfig, ReloadGate};
use crate::error::DaemonResult;
use crate::hardening::RestrictedMode;
use crate::license::{LicenseRuntime, LicenseRuntimeConfig};
use crate::north::runtime::{NorthForwardGate, NorthRuntime, NorthRuntimeConfig};
use crate::ota::OtaBootDecision;
use crate::scheduler::{GroupConfig, GroupScheduler, PollHandler, RunningScheduler};

// ---- 默认常量 ----

/// 优雅停机总宽限期（超时即中止，见模块注释）。
pub const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(30);
/// 看门狗默认巡检周期。
pub const DEFAULT_WATCHDOG_TICK: Duration = Duration::from_secs(15);
/// 心跳陈旧判定阈值（默认 3 × 看门狗周期）。
pub const DEFAULT_HEARTBEAT_STALE_AFTER: Duration = Duration::from_secs(45);
/// 配置热重载版本号轮询间隔（`ConfigShared` 是拉模型，bootstrap 负责转发为事件）。
const RELOAD_POLL_INTERVAL: Duration = Duration::from_millis(200);

// ---- 生命周期状态 ----

/// daemon 生命周期状态（经 `tokio::sync::watch` 广播；只前进不回退）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleState {
    /// 装配中（共享状态已建，组件尚未全部就绪）。
    Starting,
    /// 运行中（调度器 + 北向钩子均已触发）。
    Running,
    /// 优雅停机进行中。
    Stopping,
    /// 已停止（全部停机步骤在宽限期内完成）。
    Stopped,
}

impl LifecycleState {
    /// 状态字面量（JSON / 日志用，小写）。
    pub fn as_str(&self) -> &'static str {
        match self {
            LifecycleState::Starting => "starting",
            LifecycleState::Running => "running",
            LifecycleState::Stopping => "stopping",
            LifecycleState::Stopped => "stopped",
        }
    }
}

// ---- 共享状态 ----

/// [`DaemonShared`] 内部态（全部字段经 `Arc` 共享，`DaemonShared` 为廉价克隆句柄）。
struct DaemonSharedInner {
    /// 生命周期状态广播（单写多读）。
    state_tx: watch::Sender<LifecycleState>,
    /// 进程启动时刻（uptime 计算基准）。
    started_at: Instant,
    /// 最近一次心跳（UTC 纳秒；`0` = 尚未上报。JSON 层负责转字符串，见 mgmt 红线）。
    last_heartbeat_ns: AtomicU64,
    /// 配置快照（热重载后原子替换；读侧 `snapshot()` 无锁拿 `Arc<GatewayConfig>`）。
    config: RwLock<Arc<ConfigShared>>,
    /// 配置版本广播：热重载成功后 bootstrap 转发新版本号（mgmt SSE 消费）。
    reload_tx: watch::Sender<u64>,
    /// 停机请求：`true` = 请求优雅停机（信号监听 / 编程式 `request_shutdown` 共用）。
    shutdown_tx: watch::Sender<bool>,
    /// 优雅停机是否超宽限期被中止（诊断 / 测试观测）。
    shutdown_timed_out: AtomicBool,
    /// 北向运行期句柄（task 19 + task 54 最后一跳；`None` = 尚未启动 / 未接线）。
    ///
    /// 挂在此处而非 `run` 返回值：采集侧（pipeline / scheduler）与 mgmt 探测都
    /// 需要在运行期通过 [`DaemonShared::north_runtime`] 拿到同一句柄。
    north: RwLock<Option<Arc<NorthRuntime>>>,
    /// 授权运行期句柄（task 22/23/24 运行期接线；`None` = 未接线 / 尚未启动）。
    ///
    /// 采集侧 / mgmt / 测试据此在运行期读取授权状态（[`LicenseRuntime::state`]）与
    /// 北向转发判据（[`LicenseRuntime::north_forward_allowed`]）。
    license: RwLock<Option<Arc<LicenseRuntime>>>,
}

/// daemon 全局共享状态：生命周期 / 心跳 / 配置快照（task 51）。
///
/// 克隆廉价（内部 `Arc`）；可跨任务共享给调度器、北向与管理 API。
#[derive(Clone)]
pub struct DaemonShared {
    inner: Arc<DaemonSharedInner>,
}

impl DaemonShared {
    /// 创建共享状态：初始 `Starting`、心跳 0、默认空配置、版本 0。
    pub fn new() -> Self {
        let (state_tx, _) = watch::channel(LifecycleState::Starting);
        let (reload_tx, _) = watch::channel(0u64);
        let (shutdown_tx, _) = watch::channel(false);
        Self {
            inner: Arc::new(DaemonSharedInner {
                state_tx,
                started_at: Instant::now(),
                last_heartbeat_ns: AtomicU64::new(0),
                config: RwLock::new(Arc::new(ConfigShared::new(GatewayConfig::default()))),
                reload_tx,
                shutdown_tx,
                shutdown_timed_out: AtomicBool::new(false),
                north: RwLock::new(None),
                license: RwLock::new(None),
            }),
        }
    }

    /// 当前生命周期状态。
    pub fn state(&self) -> LifecycleState {
        *self.inner.state_tx.borrow()
    }

    /// 更新生命周期状态并广播。
    ///
    /// 必须用 `send_replace` 而非 `send`：`send` 在**无存活订阅者时会失败且新值
    /// 不落存储**（晚订阅者将读到过期状态）；watch 的语义是「最新值快照」，
    /// 状态写入必须无条件生效。
    pub fn set_state(&self, state: LifecycleState) {
        self.inner.state_tx.send_replace(state);
    }

    /// 订阅生命周期状态变化。
    pub fn subscribe_state(&self) -> watch::Receiver<LifecycleState> {
        self.inner.state_tx.subscribe()
    }

    /// 进程已运行秒数。
    pub fn uptime_secs(&self) -> u64 {
        self.inner.started_at.elapsed().as_secs()
    }

    /// 上报心跳（UTC 纳秒时间戳；只前进不回退——乱序上报取较大值）。
    pub fn touch_heartbeat(&self, utc_ns: u64) {
        self.inner
            .last_heartbeat_ns
            .fetch_max(utc_ns, Ordering::Relaxed);
    }

    /// 最近一次心跳（UTC 纳秒；`0` = 尚未上报）。
    pub fn last_heartbeat_ns(&self) -> u64 {
        self.inner.last_heartbeat_ns.load(Ordering::Relaxed)
    }

    /// 替换配置快照句柄（热重载成功后由 bootstrap 调用）。
    pub fn set_config(&self, config: Arc<ConfigShared>) {
        let mut guard = self
            .inner
            .config
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = config;
    }

    /// 当前配置共享句柄（快照随热重载原地更新）。
    pub fn config_shared(&self) -> Arc<ConfigShared> {
        self.inner
            .config
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// 当前配置快照（便捷读侧）。
    pub fn config_snapshot(&self) -> Arc<GatewayConfig> {
        self.config_shared().snapshot()
    }

    /// 当前配置版本号（每次成功热重载 +1）。
    pub fn config_version(&self) -> u64 {
        self.config_shared().version()
    }

    /// 广播一次配置重载事件（mgmt SSE 消费；`send_replace` 保证最新版本号落存储）。
    pub fn notify_config_reload(&self, version: u64) {
        self.inner.reload_tx.send_replace(version);
    }

    /// 订阅配置重载版本号。
    pub fn subscribe_config_reload(&self) -> watch::Receiver<u64> {
        self.inner.reload_tx.subscribe()
    }

    /// 请求优雅停机（等价于收到 ctrl_c / SIGTERM；`send_replace` 保证请求
    /// 即使在无订阅者窗口期也不丢失）。
    pub fn request_shutdown(&self) {
        self.inner.shutdown_tx.send_replace(true);
    }

    /// 订阅停机请求。
    pub fn subscribe_shutdown(&self) -> watch::Receiver<bool> {
        self.inner.shutdown_tx.subscribe()
    }

    /// 是否已有停机请求。
    pub fn shutdown_requested(&self) -> bool {
        *self.inner.shutdown_tx.borrow()
    }

    /// 标记停机超时被中止（内部使用，诊断可读）。
    fn mark_shutdown_timed_out(&self) {
        self.inner.shutdown_timed_out.store(true, Ordering::Relaxed);
    }

    /// 停机是否超宽限期被中止。
    pub fn shutdown_timed_out(&self) -> bool {
        self.inner.shutdown_timed_out.load(Ordering::Relaxed)
    }

    /// 挂载北向运行期句柄（bootstrap 在 Running 前调用一次）。
    ///
    /// 重复调用只替换句柄（旧句柄的驱动任务由调用方负责中止）。
    pub fn set_north_runtime(&self, runtime: Arc<NorthRuntime>) {
        let mut guard = self
            .inner
            .north
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = Some(runtime);
    }

    /// 北向运行期句柄（`None` = 未接线 / 尚未启动）。
    ///
    /// 采集侧据此投递批次（[`NorthRuntime::submit`]，同步不阻塞）；mgmt / 测试
    /// 据此观测出口与驱动统计（[`NorthRuntime::stats`]）。
    pub fn north_runtime(&self) -> Option<Arc<NorthRuntime>> {
        self.inner
            .north
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// 挂载授权运行期句柄（bootstrap 在 Running 前调用一次）。
    ///
    /// 照 [`DaemonShared::set_north_runtime`] 的「注入后只读共享」范式：
    /// 重复调用只替换句柄；未注入时保持 `None`，不 panic。
    pub fn set_license_runtime(&self, runtime: Arc<LicenseRuntime>) {
        let mut guard = self
            .inner
            .license
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = Some(runtime);
    }

    /// 授权运行期句柄（`None` = 未接线 / 尚未启动）。
    ///
    /// 采集侧 / mgmt / 测试据此读取授权状态与北向转发判据。
    pub fn license_runtime(&self) -> Option<Arc<LicenseRuntime>> {
        self.inner
            .license
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl Default for DaemonShared {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for DaemonShared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonShared")
            .field("state", &self.state())
            .field("uptime_secs", &self.uptime_secs())
            .field("last_heartbeat_ns", &self.last_heartbeat_ns())
            .field("config_version", &self.config_version())
            .field("shutdown_timed_out", &self.shutdown_timed_out())
            .field(
                "north_outlets",
                &self.north_runtime().map_or(0, |rt| rt.outlet_count()),
            )
            .field(
                "license_state",
                &self.license_runtime().map(|rt| rt.state().name()),
            )
            .finish()
    }
}

// ---- 钩子类型 ----

/// 钩子返回的 boxed future（输出 `()`；错误一律在钩子内记日志，不中断装配）。
pub type HookFuture = Pin<Box<dyn Future<Output = ()> + Send>>;
/// 北向启动钩子：Running 前调用一次，入参为共享状态句柄。
pub type NorthStarterHook = Arc<dyn Fn(DaemonShared) -> HookFuture + Send + Sync>;
/// 停机步骤钩子（北向停止 / 离线 flush / 看门狗重启共用签名）。
pub type StopHook = Arc<dyn Fn() -> HookFuture + Send + Sync>;
/// OTA 启动判定钩子的 boxed future（输出 [`OtaBootDecision`]）。
pub type OtaBootFuture = Pin<Box<dyn Future<Output = DaemonResult<OtaBootDecision>> + Send>>;
/// OTA 启动判定钩子（task 35 接线）：bootstrap 启动早期调用一次，由装配方
/// 在闭包内构造真实 `OtaManager`（store + 公钥注入）并调用
/// `boot_commit_or_rollback(healthy)`；bootstrap 只负责结果审计/日志与回滚告警。
pub type OtaBootCheckHook = Arc<dyn Fn() -> OtaBootFuture + Send + Sync>;

// ---- 装配器 ----

/// daemon 装配器：链式注入可选钩子与参数后调用 [`BootstrapBuilder::run`]。
///
/// 未注入的钩子一律 no-op（`tracing::warn!` 提示），保证装配路径永不 panic。
pub struct BootstrapBuilder {
    /// 配置文件路径（热重载监听目标）。
    config_path: PathBuf,
    /// 外部注入的共享状态（`None` = run 内部自建）。
    shared: Option<DaemonShared>,
    /// 优雅停机总宽限期。
    shutdown_grace: Duration,
    /// 看门狗巡检周期。
    watchdog_tick: Duration,
    /// 心跳陈旧判定阈值。
    heartbeat_stale_after: Duration,
    /// 是否安装真实信号监听（ctrl_c / SIGTERM）；测试传 `false` 用编程式停机。
    install_signal_handlers: bool,
    /// 南向轮询动作（`None` = 不启动调度器）。
    poll_handler: Option<Arc<dyn PollHandler>>,
    /// 北向启动钩子。
    north_starter: Option<NorthStarterHook>,
    /// 北向停止钩子。
    north_stopper: Option<StopHook>,
    /// 离线队列 flush 钩子。
    offline_flusher: Option<StopHook>,
    /// 看门狗重启钩子（单次）。
    watchdog_restart: Option<StopHook>,
    /// OTA 启动判定钩子（task 35 接线；`None` = no-op warn）。
    ota_boot_check: Option<OtaBootCheckHook>,
    /// 北向运行期装配输入（task 19 + task 54 最后一跳；`None` = 未接线，仅 warn）。
    north: Option<NorthRuntimeConfig>,
    /// 授权运行期装配输入（task 22/23/24；`None` = 未接线，跳过授权编排）。
    license: Option<LicenseRuntimeConfig>,
}

impl BootstrapBuilder {
    /// 以配置文件路径创建装配器（其余参数取默认值）。
    pub fn new(config_path: impl Into<PathBuf>) -> Self {
        Self {
            config_path: config_path.into(),
            shared: None,
            shutdown_grace: DEFAULT_SHUTDOWN_GRACE,
            watchdog_tick: DEFAULT_WATCHDOG_TICK,
            heartbeat_stale_after: DEFAULT_HEARTBEAT_STALE_AFTER,
            install_signal_handlers: true,
            poll_handler: None,
            north_starter: None,
            north_stopper: None,
            offline_flusher: None,
            watchdog_restart: None,
            ota_boot_check: None,
            north: None,
            license: None,
        }
    }

    /// 注入外部共享状态（mgmt / 北向在运行期需要同一句柄时使用；
    /// 未注入则 run 内部自建并在结束时返回）。
    pub fn with_shared(mut self, shared: DaemonShared) -> Self {
        self.shared = Some(shared);
        self
    }

    /// 覆盖优雅停机总宽限期。
    pub fn with_shutdown_grace(mut self, grace: Duration) -> Self {
        self.shutdown_grace = grace;
        self
    }

    /// 覆盖看门狗巡检周期。
    pub fn with_watchdog_tick(mut self, tick: Duration) -> Self {
        self.watchdog_tick = tick;
        self
    }

    /// 覆盖心跳陈旧判定阈值。
    pub fn with_heartbeat_stale_after(mut self, stale: Duration) -> Self {
        self.heartbeat_stale_after = stale;
        self
    }

    /// 关闭真实信号监听（测试用：以 [`DaemonShared::request_shutdown`] 编程式停机）。
    pub fn without_signal_handlers(mut self) -> Self {
        self.install_signal_handlers = false;
        self
    }

    /// 注入南向轮询动作（调度器组按配置点位表自动推导，见 [`build_groups`]）。
    pub fn with_poll_handler(mut self, handler: Arc<dyn PollHandler>) -> Self {
        self.poll_handler = Some(handler);
        self
    }

    /// 注入北向启动钩子（缺省 no-op warn）。
    pub fn with_north_starter(mut self, hook: NorthStarterHook) -> Self {
        self.north_starter = Some(hook);
        self
    }

    /// 注入北向停止钩子（缺省 no-op warn）。
    pub fn with_north_stopper(mut self, hook: StopHook) -> Self {
        self.north_stopper = Some(hook);
        self
    }

    /// 注入北向运行期装配输入（task 19 + task 54 的**最后一跳**）。
    ///
    /// Running 前 bootstrap 会按配置里的 `[[outlets]]` 为每个出口构造
    /// `MqttClient` + `NorthOutlet`（有界发送队列 / PUBACK 门控确认 / 补发幂等 /
    /// 审计取走上报）并启动驱动任务；句柄挂到 [`DaemonShared::north_runtime`]，
    /// 停机步骤 3 中止全部驱动任务。
    ///
    /// 未注入时北向**不启动**：配置里声明了 `[[outlets]]` 则记 `error!`
    /// （绝不静默放过），否则记 `info!`。
    pub fn with_north_runtime(mut self, cfg: NorthRuntimeConfig) -> Self {
        self.north = Some(cfg);
        self
    }

    /// 注入授权运行期装配输入（task 22/23/24 运行期接线）。
    ///
    /// Running 前 bootstrap 会构造 [`LicenseRuntime`]、启动其 `step` 循环并把句柄挂到
    /// [`DaemonShared::license_runtime`]；停机步骤通知该循环优雅退出。
    ///
    /// 未注入时授权编排**不启动**：`DaemonShared::license_runtime()` 保持 `None`，
    /// 其余装配行为与现状完全一致（纯 UI / 库测试场景不受影响）。
    pub fn with_license_runtime(mut self, cfg: LicenseRuntimeConfig) -> Self {
        self.license = Some(cfg);
        self
    }

    /// 注入离线队列 flush 钩子（缺省 no-op + TODO，见模块注释限制 3）。
    pub fn with_offline_flusher(mut self, hook: StopHook) -> Self {
        self.offline_flusher = Some(hook);
        self
    }

    /// 注入看门狗重启钩子（缺省 no-op warn，仅触发一次）。
    pub fn with_watchdog_restart(mut self, hook: StopHook) -> Self {
        self.watchdog_restart = Some(hook);
        self
    }

    /// 注入 OTA 启动判定钩子（task 35 接线；缺省 no-op warn）。
    ///
    /// 钩子在 bootstrap 启动早期（配置加载前）被调用一次；装配方在闭包内
    /// 构造真实 `OtaManager` 并调用 `boot_commit_or_rollback(healthy)`，
    /// 判定结果由 bootstrap 写审计/日志：`RolledBackTo` 触发显式 `[WARN]`。
    pub fn with_ota_boot_check(mut self, hook: OtaBootCheckHook) -> Self {
        self.ota_boot_check = Some(hook);
        self
    }

    /// 装配并运行 daemon 直至优雅停机完成，返回共享状态句柄。
    ///
    /// 装配顺序：共享状态 → 配置热重载 → 调度器 → 北向钩子 → Running；
    /// 停机顺序：调度器 → flush → north → Stopped。
    ///
    /// # Errors
    /// 配置初始加载 / watcher 初始化失败 → [`DaemonError`]（此时未产生任何后台任务）。
    pub async fn run(mut self) -> DaemonResult<DaemonShared> {
        // ① 初始化共享状态。
        let shared = self.shared.clone().unwrap_or_default();
        shared.set_state(LifecycleState::Starting);
        info!("bootstrap: daemon starting");

        // ①-a 完整性自检（task 50 接线）：早期执行、fail-safe 不 panic、
        // 不中断启动；进入 RestrictedMode 仅打 [WARN] 并继续（敏感能力的
        // 降级处置由 mgmt / 北向按受限标记执行）。
        if let Some(restricted) = startup_integrity_check() {
            warn!(
                reason = ?restricted.reason(),
                "bootstrap: [WARN] RestrictedMode: integrity self-check did not pass; \
                 continuing in restricted mode (fail-safe)"
            );
        }

        // ①-b OTA 启动判定（task 35 接线）：commit / 回滚结果写审计日志；
        //      回滚发生时显式 [WARN] 并记录原因（宽限期内未确认启动健康）。
        run_ota_boot_check(&self.ota_boot_check).await;

        // ①-c 授权运行期（**提前到配置 / 北向之前**）：北向转发门控与免费版配额
        //     闸门都需要授权状态真相源（[`LicenseRuntime`]），故先构造并步进一次
        //     确定初始状态（Trial / Licensed / Degraded）；随后 step 循环照常由
        //     [`LicenseRuntime::spawn`] 驱动。未注入时跳过（行为与既往一致）。
        let license_runtime: Option<Arc<LicenseRuntime>> = match self.license.take() {
            Some(cfg) => {
                let runtime = Arc::new(LicenseRuntime::new(cfg));
                // 装配期步进一次：启动阶段（试用判定）+ 激活恢复 + 心跳/倒计时，
                // 让后续的配额闸门读到真实初始状态而非占位 Unlicensed。
                if let Err(err) = runtime.step().await {
                    warn!(error = %err, "bootstrap: license runtime initial step failed");
                }
                shared.set_license_runtime(Arc::clone(&runtime));
                // 常驻 step 循环（首个 tick 立即触发；停机序列经 shutdown() 收口）。
                let _driver = Arc::clone(&runtime).spawn();
                info!("bootstrap: license runtime started");
                Some(runtime)
            }
            None => {
                info!("bootstrap: license runtime not injected (no licensing orchestration)");
                None
            }
        };

        // ② 配置热重载：初始加载失败直接返回错误（绝不静默空配置）。
        //     热重载准入闸门（免费版配额，fail-closed）：Degraded 期间设备数 /
        //     协议 / 采集间隔超限的重载一律拒绝（保留旧快照 + warn 可解释）。
        let (reloader, config_shared) = match &license_runtime {
            Some(license) => {
                let gate: ReloadGate = {
                    let license = Arc::clone(license);
                    Arc::new(move |config: &GatewayConfig| {
                        license
                            .enforce_free_limits(config)
                            .map_err(|err| err.to_string())
                    })
                };
                ConfigHotReloader::spawn_with_gate(&self.config_path, Some(gate))?
            }
            None => ConfigHotReloader::spawn(&self.config_path)?,
        };
        shared.set_config(config_shared.clone());
        shared.notify_config_reload(config_shared.version());

        // ②-b 免费版配额闸门（**启动装配期**，fail-closed）：Degraded（免费版）
        //     状态下配置超额（设备数 > 8 / 非 Modbus / 间隔 < 1s）→ 拒绝启动，
        //     错误含字段名 / 标识值与恢复路径。闸门通过后 reloader 继续服务热重载。
        if let Some(license) = &license_runtime {
            if let Err(err) = license.enforce_free_limits(&config_shared.snapshot()) {
                reloader.stop();
                error!(
                    error = %err,
                    "bootstrap: [ERROR] config rejected by free-edition quota gate; \
                     aborting startup"
                );
                return Err(err);
            }
        }

        // 转发任务：ConfigShared 是拉模型，这里轮询版本号并转发为共享态事件。
        let reload_task = tokio::spawn(forward_config_reload(
            shared.clone(),
            config_shared.clone(),
            shared.subscribe_shutdown(),
        ));

        // ③ 调度器：组按配置点位表推导（一组一设备，周期 = 该设备最小采集频率）。
        let groups = build_groups(&config_shared.snapshot());
        let running_scheduler = match &self.poll_handler {
            Some(handler) if !groups.is_empty() => {
                match GroupScheduler::new(
                    DynPollHandler {
                        inner: handler.clone(),
                    },
                    groups,
                ) {
                    Ok(scheduler) => {
                        let running = scheduler.start();
                        info!(
                            groups = ?running.group_names(),
                            "bootstrap: scheduler started"
                        );
                        Some(running)
                    }
                    Err(err) => {
                        // 组校验失败只降级不 panic：采集缺失由看门狗 / 诊断兜底。
                        error!(error = %err, "bootstrap: scheduler build failed; running without scheduler");
                        None
                    }
                }
            }
            Some(_) => {
                warn!("bootstrap: poll handler present but config has no device groups; scheduler not started");
                None
            }
            None => {
                warn!("bootstrap: no poll handler configured; scheduler not started (V1)");
                None
            }
        };

        // ④ 北向启动（task 19 + task 54 的**最后一跳**）：把 `MqttClient` +
        //    `NorthOutlet` 真正接到运行期，替换原先的 no-op 占位。
        //
        // 注入 `NorthRuntimeConfig` 后，按配置 `[[outlets]]` 为每个出口构造
        // `MqttClient` + `NorthOutlet`（有界发送队列 / PUBACK 门控确认 / 补发幂等 /
        // 审计取走上报）并起驱动任务；句柄挂到 [`DaemonShared::north_runtime`]，
        // 供采集侧在运行期投递（`NorthRuntime::submit`）、mgmt 观测；停机步骤 3
        // 中止全部驱动任务。**未注入**时按是否声明 `[[outlets]]` 记 error/info
        // （绝不静默放过已声明的出口）。
        match self.north.take() {
            Some(mut cfg) => {
                // 北向转发授权闸门：把授权状态真相源接到每个出口的驱动任务
                //（Degraded / Unlicensed ⇒ 跳过发送；恢复后下一拍自动继续）。
                if let Some(license) = &license_runtime {
                    cfg.set_gate(Arc::clone(license) as Arc<dyn NorthForwardGate>);
                }
                let snapshot = config_shared.snapshot();
                let runtime = Arc::new(NorthRuntime::start(
                    snapshot.gateway.gateway_id.as_str(),
                    &snapshot.outlets,
                    cfg,
                    shared.subscribe_shutdown(),
                ));
                let stats = runtime.stats();
                if stats.skipped > 0 {
                    error!(
                        started = stats.outlets,
                        skipped = stats.skipped,
                        "bootstrap: [ERROR] some north outlets were rejected at startup \
                         (invalid broker / unsupported TLS); northbound is degraded there"
                    );
                }
                info!(
                    outlets = stats.outlets,
                    names = ?runtime.outlet_names(),
                    "bootstrap: north runtime started"
                );
                shared.set_north_runtime(runtime);
            }
            None => {
                let outlets = config_shared.snapshot().outlets.len();
                if outlets == 0 {
                    info!("bootstrap: north runtime not injected; no [[outlets]] declared");
                } else {
                    error!(
                        outlets,
                        "bootstrap: [ERROR] [[outlets]] declared but north runtime not injected; \
                         northbound will NOT send (call BootstrapBuilder::with_north_runtime)"
                    );
                }
            }
        }
        // 可选北向启动扩展钩子（运行期就绪后调用一次；缺省 no-op warn，向后兼容）。
        match &self.north_starter {
            Some(hook) => hook(shared.clone()).await,
            None => {
                warn!("bootstrap: north_starter hook not configured (optional extension point)")
            }
        }

        // ④-b 授权运行期已在 ①-c 接线（见上）：构造、装配期步进、句柄挂载均在
        //     北向之前完成（北向门控与配额闸门依赖授权状态真相源）。此处仅保留
        //     north_starter 扩展钩子。

        shared.set_state(LifecycleState::Running);
        info!("bootstrap: daemon running");

        // ⑤ 看门狗。
        let watchdog_task = tokio::spawn(run_watchdog(
            shared.clone(),
            self.watchdog_tick,
            self.heartbeat_stale_after,
            self.watchdog_restart.clone(),
            shared.subscribe_shutdown(),
        ));

        // ⑥ 等待停机信号（ctrl_c / SIGTERM / 编程式请求）。
        if self.install_signal_handlers {
            wait_for_signal(&shared).await;
        } else {
            let mut shutdown_rx = shared.subscribe_shutdown();
            while !*shutdown_rx.borrow_and_update() {
                if shutdown_rx.changed().await.is_err() {
                    break;
                }
            }
        }

        // ⑦ 优雅停机（顺序固定，宽限期兜底）。
        graceful_shutdown(&shared, &self, running_scheduler).await;

        // ⑧ 清理后台任务与热重载线程。
        watchdog_task.abort();
        reload_task.abort();
        reloader.stop();

        info!("bootstrap: daemon stopped");
        Ok(shared)
    }
}

/// `Arc<dyn PollHandler>` 适配器：[`GroupScheduler`] 要求 `H: PollHandler` 具体类型，
/// 动态分发的 handler 需要一层薄包装（委托调用，零语义增量）。
struct DynPollHandler {
    inner: Arc<dyn PollHandler>,
}

#[async_trait]
impl PollHandler for DynPollHandler {
    async fn poll(&self, group: &str, point_ids: &[String]) -> DaemonResult<usize> {
        self.inner.poll(group, point_ids).await
    }
}

impl std::fmt::Debug for DynPollHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DynPollHandler").finish_non_exhaustive()
    }
}

/// OTA 启动判定（task 35 接线）：调用注入的判定钩子并按结果写审计/日志。
///
/// - `NoPending` / `Committed` → `info!`；
/// - `RolledBackTo` → **显式 `[WARN]`**（原因：新版本宽限期内未确认启动健康，
///   pending 已清除、旧版本保留）；
/// - 钩子返回 Err → `error!` 后继续启动（OTA 判定不可用不应阻断数据面；
///   失败不静默）；
/// - 未注入钩子 → `warn!`（no-op，与其它钩子的缺省语义一致）。
async fn run_ota_boot_check(hook: &Option<OtaBootCheckHook>) {
    match hook {
        Some(hook) => match hook().await {
            Ok(OtaBootDecision::NoPending) => {
                info!("bootstrap: ota boot check: no pending firmware (no-op)");
            }
            Ok(OtaBootDecision::Committed { version }) => {
                info!(
                    version,
                    "bootstrap: ota boot commit: pending firmware promoted"
                );
            }
            Ok(OtaBootDecision::RolledBackTo { version }) => {
                warn!(
                    version,
                    "bootstrap: [WARN] ota boot ROLLBACK: pending firmware was not \
                     confirmed healthy within the grace period; pending slot cleared, \
                     previous version retained"
                );
            }
            Err(err) => {
                error!(
                    error = %err,
                    "bootstrap: ota boot check failed; continuing without boot decision"
                );
            }
        },
        None => warn!("bootstrap: ota boot check not configured (no-op hook)"),
    }
}

/// 完整性 manifest 文件名约定：`<exe 全名>.integrity-manifest`，内容为
/// 64 字符小写 hex 的 exe SHA-256（task 58 打包期注入点；缺失即受限）。
#[cfg_attr(debug_assertions, allow(dead_code))] // debug 构建下自检被跳过，仅 release 引用
const INTEGRITY_MANIFEST_SUFFIX: &str = ".integrity-manifest";

/// 从 manifest 文件读取 expected 哈希（64 hex → 32 字节）。
///
/// 文件缺失 / 非 64 hex / 读取失败 → `None`（调用方按 `Unavailable` 处理，
/// fail-safe 进入受限模式，绝不 panic）。
#[cfg_attr(debug_assertions, allow(dead_code))] // debug 构建下仅测试使用
fn read_expected_exe_hash(manifest: &Path) -> Option<[u8; 32]> {
    let text = std::fs::read_to_string(manifest).ok()?;
    let trimmed = text.trim();
    if trimmed.len() != 64 || !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut expected = [0u8; 32];
    // 长度与字符集已校验，逐字节配对解码不会越界（防御式写法，零 panic）。
    for (i, slot) in expected.iter_mut().enumerate() {
        let hi = i * 2;
        let lo = hi + 1;
        let hi_val = (trimmed.as_bytes()[hi] as char).to_digit(16)?;
        let lo_val = (trimmed.as_bytes()[lo] as char).to_digit(16)?;
        *slot = ((hi_val << 4) | lo_val) as u8;
    }
    Some(expected)
}

/// 启动期完整性自检（task 50 接线）。
///
/// ## debug 构建跳过（有意为之，勿“补全”）
/// debug / 测试构建的 exe 每次重编译哈希都不同，且测试进程的 expected 哈希
/// 无法在构建期注入 —— 此时强制执行自检必然全员 `Unavailable`（假受限）。
/// 故 debug 构建下**直接跳过**并返回 `None`；release 构建下从 exe 同目录的
/// manifest 读取 expected 哈希（task 58 打包期注入），manifest 缺失即
/// `Unavailable` → 受限模式（fail-safe：拿不到完整性证据按可疑处理）。
fn startup_integrity_check() -> Option<RestrictedMode> {
    #[cfg(debug_assertions)]
    {
        // debug 构建：跳过（理由见函数文档）。
        None
    }
    #[cfg(not(debug_assertions))]
    {
        let verdict = match std::env::current_exe() {
            Ok(exe) => {
                let manifest = exe.with_file_name(format!(
                    "{}{INTEGRITY_MANIFEST_SUFFIX}",
                    exe.file_name().and_then(|n| n.to_str()).unwrap_or_default()
                ));
                match read_expected_exe_hash(&manifest) {
                    Some(expected) => crate::hardening::verify_self_integrity(&expected),
                    None => crate::hardening::IntegrityVerdict::Unavailable {
                        reason: format!(
                            "integrity manifest missing or invalid: {}",
                            manifest.display()
                        ),
                    },
                }
            }
            Err(e) => crate::hardening::IntegrityVerdict::Unavailable {
                reason: format!("current_exe: {e}"),
            },
        };
        RestrictedMode::from_integrity(&verdict)
    }
}

/// 信号等待：ctrl_c +（unix）SIGTERM + 编程式停机请求，任一触发即返回。
async fn wait_for_signal(shared: &DaemonShared) {
    let mut shutdown_rx = shared.subscribe_shutdown();
    #[cfg(unix)]
    {
        let mut sigterm =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(sigterm) => sigterm,
                // 信号监听安装失败（受限环境）：降级为只等 ctrl_c 与编程式请求。
                Err(err) => {
                    warn!(error = %err, "bootstrap: SIGTERM listener unavailable");
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {}
                        _ = shutdown_rx.changed() => {}
                    }
                    return;
                }
            };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("bootstrap: ctrl-c received");
            }
            _ = sigterm.recv() => {
                info!("bootstrap: SIGTERM received");
            }
            _ = shutdown_rx.changed() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = &mut shutdown_rx;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("bootstrap: ctrl-c received");
            }
            _ = shutdown_rx.changed() => {}
        }
    }
}

/// 优雅停机序列（严格顺序 + 宽限期兜底）。
///
/// 超时即中止：`timeout` 丢弃 steps future，`RunningScheduler` 的 `Drop`
/// 会幂等 abort 全部组任务；state 保持在 `Stopping` 并打 `shutdown_timed_out` 标志。
async fn graceful_shutdown(
    shared: &DaemonShared,
    builder: &BootstrapBuilder,
    scheduler: Option<RunningScheduler>,
) {
    shared.set_state(LifecycleState::Stopping);
    info!("bootstrap: graceful shutdown started");
    let steps = shutdown_sequence(shared, builder, scheduler);
    match tokio::time::timeout(builder.shutdown_grace, steps).await {
        Ok(()) => {
            shared.set_state(LifecycleState::Stopped);
            info!("bootstrap: graceful shutdown completed");
        }
        Err(_) => {
            error!(
                grace_secs = builder.shutdown_grace.as_secs(),
                "bootstrap: graceful shutdown exceeded grace period; aborting remaining steps"
            );
            shared.mark_shutdown_timed_out();
        }
    }
}

/// 停机三步骤：调度器 → 离线队列 flush → 北向停止。
///
/// 每步之间 `yield_now`：给 watch 订阅方（mgmt SSE / 测试断言）一个确定的观察点。
async fn shutdown_sequence(
    shared: &DaemonShared,
    builder: &BootstrapBuilder,
    scheduler: Option<RunningScheduler>,
) {
    // ① 调度器停：中止并等待全部组任务退出。
    if let Some(scheduler) = scheduler {
        scheduler.shutdown().await;
    }
    info!("bootstrap: shutdown step 1/3 scheduler stopped");
    tokio::task::yield_now().await;

    // ② 离线队列 flush。
    //
    // TODO(task 17/18 集成): bootstrap 装配 `OfflineQueue` 实例后，把本钩子替换为
    // 直接调用 `OfflineQueue::flush()`（同步 API，返回落盘批数）。V1 为 best-effort：
    // 未接线时仅记录警告，不阻塞停机（队列自身有 WAL / 落盘兜底，见 offline_queue.rs）。
    match &builder.offline_flusher {
        Some(hook) => hook().await,
        None => warn!(
            "bootstrap: offline queue flush not wired (no-op); TODO when OfflineQueue integrated"
        ),
    }
    info!("bootstrap: shutdown step 2/3 offline queue flushed");
    tokio::task::yield_now().await;

    // ③ 北向停止：先中止全部驱动任务（幂等、不阻塞宽限期），再调可选停止钩子。
    if let Some(runtime) = shared.north_runtime() {
        runtime.shutdown();
        info!(
            outlets = runtime.outlet_count(),
            "bootstrap: north runtime driver tasks aborted"
        );
    }
    match &builder.north_stopper {
        Some(hook) => hook().await,
        None => warn!("bootstrap: north_stopper not configured (no-op hook)"),
    }
    info!("bootstrap: shutdown step 3/3 northbound stopped");
    tokio::task::yield_now().await;

    // ④ 授权运行期停机：通知其 `step` 循环优雅退出（幂等、不阻塞宽限期）。
    if let Some(runtime) = shared.license_runtime() {
        runtime.shutdown();
        info!(
            state = runtime.state().name(),
            "bootstrap: license runtime shutdown signalled"
        );
    }
    tokio::task::yield_now().await;
}

/// 看门狗主循环：按 tick 巡检心跳陈旧度，异常时 `error!` 并（单次）触发重启钩子。
///
/// V1 限制见模块注释 1/2：无心跳上报（= 0）不判定；重启只调一次钩子。
async fn run_watchdog(
    shared: DaemonShared,
    tick: Duration,
    stale_after: Duration,
    restart_hook: Option<StopHook>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut restart_attempted = false;
    // tokio interval 首拍立即完成，先消费掉再进巡检循环。
    let mut ticker = tokio::time::interval(tick);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    ticker.tick().await;
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = shutdown.changed() => break,
        }
        let last = shared.last_heartbeat_ns();
        if last == 0 {
            // 尚无任何心跳上报：无法判定（V1 限制 1），不误报。
            continue;
        }
        let now_ns = utc_now_ns();
        let age_ns = now_ns.saturating_sub(last);
        if age_ns <= stale_after.as_nanos() as u64 {
            continue;
        }
        error!(
            age_ms = age_ns / 1_000_000,
            stale_after_ms = stale_after.as_millis() as u64,
            "watchdog: heartbeat stale; southbound pipeline may be stalled"
        );
        if restart_attempted {
            warn!("watchdog: restart already attempted once; giving up (V1 limitation)");
            continue;
        }
        restart_attempted = true;
        match &restart_hook {
            Some(hook) => {
                warn!("watchdog: attempting one restart via hook (V1: single attempt)");
                hook().await;
            }
            None => warn!(
                "watchdog: restart hook not configured; record-only (V1 limitation, see module doc)"
            ),
        }
    }
}

/// 当前 UTC 纳秒时间戳（Unix 纪元起）。
fn utc_now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        // 时钟早于纪元（极端环境）：按 0 处理，看门狗据「= 0 未上报」跳过判定。
        .unwrap_or(0)
}

/// 配置重载转发任务：轮询 `ConfigShared` 版本号，变化时刷新共享态并广播事件。
async fn forward_config_reload(
    shared: DaemonShared,
    config_shared: Arc<ConfigShared>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut last_version = config_shared.version();
    let mut ticker = tokio::time::interval(RELOAD_POLL_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = shutdown.changed() => break,
        }
        let version = config_shared.version();
        if version != last_version {
            last_version = version;
            // ConfigShared 内部原地换快照，这里刷新共享态句柄并对外广播。
            shared.set_config(config_shared.clone());
            shared.notify_config_reload(version);
            info!(
                version,
                "bootstrap: config reload propagated to DaemonShared"
            );
        }
    }
}

/// 按配置点位表推导调度器组：一组一设备，周期 = 该设备最小采集频率，点位去重保序。
///
/// 组校验失败（理论上不会发生）时记 warn 并跳过该组（错误隔离，不阻塞其余组）。
fn build_groups(config: &GatewayConfig) -> Vec<GroupConfig> {
    let mut order: Vec<String> = Vec::new();
    // device_id -> (最小频率 ms, 去重后的点位列表)；order 保持设备首次出现顺序。
    let mut devices: std::collections::HashMap<String, (u64, Vec<String>)> =
        std::collections::HashMap::new();
    for point in &config.points {
        let entry = devices.entry(point.device_id.clone()).or_insert_with(|| {
            order.push(point.device_id.clone());
            (u64::MAX, Vec::new())
        });
        entry.0 = entry.0.min(point.frequency_ms.max(1));
        if !entry.1.contains(&point.point_id) {
            entry.1.push(point.point_id.clone());
        }
    }
    order
        .iter()
        .filter_map(|device_id| {
            let (freq_ms, point_ids) = devices.get(device_id)?;
            match GroupConfig::new(
                device_id.clone(),
                Duration::from_millis(*freq_ms),
                point_ids.clone(),
            ) {
                Ok(group) => Some(group),
                Err(err) => {
                    warn!(device_id, error = %err, "bootstrap: skip invalid group");
                    None
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use crate::auth::client::{LicensingClient, LicensingClientConfig};
    use crate::auth::machine_id::{FingerprintKey, MachineIdentity, StaticAnchor};
    use crate::auth::signing::{AuthSigner, LicenseGate, StaticKeyProvider};

    /// 最小可加载配置（ConfigHotReloader 要求真实文件）。
    const TEST_TOML: &str = r#"
[gateway]
gateway_id = "gw-test"

[[outlets]]
name = "north-1"
broker = "mqtt://127.0.0.1:1883"

[[points]]
device_id = "dev-01"
point_id = "p1"
protocol = "modbus-tcp"
address = "127.0.0.1:502"
frequency_ms = 100

[[points]]
device_id = "dev-01"
point_id = "p2"
protocol = "modbus-tcp"
address = "127.0.0.1:502"
frequency_ms = 500
"#;

    /// 把测试配置写入临时目录并返回文件路径。
    fn write_test_config(dir: &tempfile::TempDir) -> PathBuf {
        let path = dir.path().join("config.toml");
        std::fs::write(&path, TEST_TOML).expect("write test config");
        path
    }

    /// 构造记录执行的钩子：调用时向共享日志 push 一个标记。
    fn recording_hook(log: &Arc<Mutex<Vec<String>>>, tag: &'static str) -> StopHook {
        let log = log.clone();
        Arc::new(move || {
            if let Ok(mut guard) = log.lock() {
                guard.push(tag.to_string());
            }
            Box::pin(async {}) as HookFuture
        })
    }

    /// 轮询等待共享态到达指定状态（current_thread 运行时，yield 让 run 任务推进）。
    ///
    /// 若 run 任务提前结束（大概率装配返回 Err，如配置加载失败），取出其真实
    /// 返回值放进 panic 消息，避免「卡在 Starting」的无因失败。
    async fn wait_for_state(
        handle: tokio::task::JoinHandle<DaemonResult<DaemonShared>>,
        shared: &DaemonShared,
        want: LifecycleState,
    ) -> tokio::task::JoinHandle<DaemonResult<DaemonShared>> {
        for _ in 0..2000 {
            if shared.state() == want {
                return handle;
            }
            tokio::task::yield_now().await;
        }
        if handle.is_finished() {
            let result = handle.await;
            panic!(
                "run task finished before reaching {want:?}: {:?}",
                result.map(|r| r.map(|s| s.state().as_str().to_string()))
            );
        }
        panic!(
            "state did not reach {want:?} (current {:?})",
            shared.state()
        );
    }

    /// QA Happy: 状态迁移顺序 Starting → Running → Stopping → Stopped 严格可观测；
    /// 北向启动钩子在 Running 前触发；停机顺序 flush（2/3）→ north（3/3）。
    #[tokio::test(start_paused = true)]
    async fn lifecycle_states_and_shutdown_order_observed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

        let north_log = log.clone();
        let north_starter: NorthStarterHook = Arc::new(move |_shared| {
            if let Ok(mut guard) = north_log.lock() {
                guard.push("north-start".to_string());
            }
            Box::pin(async {}) as HookFuture
        });

        let shared = DaemonShared::new();
        let mut state_rx = shared.subscribe_state();
        // 订阅时初始值已可见：Starting。
        assert_eq!(*state_rx.borrow_and_update(), LifecycleState::Starting);

        let builder = BootstrapBuilder::new(write_test_config(&dir))
            .with_shared(shared.clone())
            .without_signal_handlers()
            .with_north_starter(north_starter)
            .with_offline_flusher(recording_hook(&log, "flush"))
            .with_north_stopper(recording_hook(&log, "north-stop"));

        let handle = wait_for_state(
            tokio::spawn(builder.run()),
            &shared,
            LifecycleState::Running,
        )
        .await;

        // Running 事件可被订阅端观测到（watch 的 changed 标志不丢失）。
        assert!(state_rx.changed().await.is_ok());
        assert_eq!(*state_rx.borrow_and_update(), LifecycleState::Running);

        // 独立任务收集 Stopping / Stopped（run 的每步 yield_now 提供确定观察点）。
        let collector = tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Ok(()) = state_rx.changed().await {
                let state = *state_rx.borrow_and_update();
                seen.push(state);
                if state == LifecycleState::Stopped {
                    break;
                }
            }
            seen
        });

        shared.request_shutdown();
        let result = handle.await.expect("run task joins").expect("run ok");

        assert_eq!(
            collector.await.expect("collector joins"),
            vec![LifecycleState::Stopping, LifecycleState::Stopped],
            "stopping phase must be observed in order"
        );
        assert_eq!(result.state(), LifecycleState::Stopped);
        assert!(!result.shutdown_timed_out(), "grace period not exceeded");

        // 钩子执行顺序：north-start（装配期）→ flush（停机 2/3）→ north-stop（停机 3/3）。
        let executed = log.lock().expect("log lock").clone();
        assert_eq!(
            executed,
            vec![
                "north-start".to_string(),
                "flush".to_string(),
                "north-stop".to_string()
            ],
            "hook execution order"
        );
    }

    /// QA Error: 停机步骤卡死 → 宽限期超时，error 路径中止：
    /// state 保持在 Stopping（不谎报 Stopped）、timed_out 标志置位、run 正常返回。
    #[tokio::test(start_paused = true)]
    async fn shutdown_timeout_aborts_and_marks_timed_out() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let shared = DaemonShared::new();

        // 北向停止钩子虚拟时间睡 10s（远超 200ms 宽限期）→ 模拟卡死。
        let north_stopper: StopHook = Arc::new(|| {
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(10)).await;
            }) as HookFuture
        });

        let builder = BootstrapBuilder::new(write_test_config(&dir))
            .with_shared(shared.clone())
            .without_signal_handlers()
            .with_shutdown_grace(Duration::from_millis(200))
            .with_offline_flusher(recording_hook(&log, "flush"))
            .with_north_stopper(north_stopper);

        let handle = wait_for_state(
            tokio::spawn(builder.run()),
            &shared,
            LifecycleState::Running,
        )
        .await;

        shared.request_shutdown();
        // 推进虚拟时间直到 run 完成（宽限期 200ms 由超时计时器触发）。
        for _ in 0..60 {
            if handle.is_finished() {
                break;
            }
            tokio::time::advance(Duration::from_millis(50)).await;
        }
        let result = handle
            .await
            .expect("run task joins")
            .expect("run returns Ok even on timeout");

        // flush（2/3）已执行；north 停止卡死被超时中止。
        assert!(
            log.lock().expect("log lock").contains(&"flush".to_string()),
            "flush step must have run before the hang"
        );
        assert_eq!(
            result.state(),
            LifecycleState::Stopping,
            "must NOT claim Stopped after aborting"
        );
        assert!(result.shutdown_timed_out(), "timed_out flag must be set");
    }

    /// QA: 看门狗按 tick 巡检心跳陈旧度；陈旧时 error + 单次重启钩子（不重复触发）。
    ///
    /// 陈旧度基于 UTC 纳秒墙钟：测试以「过去时间戳」注入心跳，天然满足陈旧条件，
    /// 与虚拟时间推进解耦（确定性）。
    #[tokio::test(start_paused = true)]
    async fn watchdog_triggers_single_restart_on_stale_heartbeat() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let shared = DaemonShared::new();

        let builder = BootstrapBuilder::new(write_test_config(&dir))
            .with_shared(shared.clone())
            .without_signal_handlers()
            .with_watchdog_tick(Duration::from_millis(100))
            .with_heartbeat_stale_after(Duration::from_millis(200))
            .with_watchdog_restart(recording_hook(&log, "restart"));

        let handle = wait_for_state(
            tokio::spawn(builder.run()),
            &shared,
            LifecycleState::Running,
        )
        .await;

        // 注入「10s 前」的心跳 → 相对当前墙钟必然陈旧（> 200ms 阈值）。
        let stale_ns = utc_now_ns().saturating_sub(10_000_000_000);
        shared.touch_heartbeat(stale_ns);

        // 推进虚拟时间驱动看门狗 tick（首拍已消费，100ms 一拍）。
        for _ in 0..10 {
            if log
                .lock()
                .expect("log lock")
                .contains(&"restart".to_string())
            {
                break;
            }
            tokio::time::advance(Duration::from_millis(100)).await;
        }
        assert_eq!(
            log.lock()
                .expect("log lock")
                .iter()
                .filter(|tag| *tag == "restart")
                .count(),
            1,
            "restart hook must fire exactly once"
        );

        // 继续巡检多个 tick：重启不重复触发（V1 单次限制）。
        for _ in 0..5 {
            tokio::time::advance(Duration::from_millis(100)).await;
        }
        assert_eq!(
            log.lock()
                .expect("log lock")
                .iter()
                .filter(|tag| *tag == "restart")
                .count(),
            1,
            "restart must not repeat"
        );

        shared.request_shutdown();
        handle.await.expect("run task joins").expect("run ok");
    }

    /// QA: 心跳只前进不回退（乱序上报取较大值）。
    #[test]
    fn touch_heartbeat_is_monotonic() {
        let shared = DaemonShared::new();
        assert_eq!(shared.last_heartbeat_ns(), 0, "no heartbeat yet");
        shared.touch_heartbeat(1_000);
        shared.touch_heartbeat(500);
        assert_eq!(shared.last_heartbeat_ns(), 1_000, "must keep the max");
        shared.touch_heartbeat(2_000);
        assert_eq!(shared.last_heartbeat_ns(), 2_000);
    }

    /// QA: build_groups 一组一设备、周期取设备最小采集频率、点位去重保序。
    #[test]
    fn build_groups_derives_one_group_per_device_with_min_frequency() {
        let config = GatewayConfig::parse(TEST_TOML).expect("parse");
        let groups = build_groups(&config);
        assert_eq!(groups.len(), 1, "one device → one group");
        let group = &groups[0];
        assert_eq!(group.name, "dev-01");
        assert_eq!(
            group.interval,
            Duration::from_millis(100),
            "min(100, 500) = 100"
        );
        assert_eq!(group.point_ids, vec!["p1".to_string(), "p2".to_string()]);
    }

    /// QA: 多设备各自成组且顺序保持首次出现顺序；未知设备无组。
    #[test]
    fn build_groups_preserves_device_order() {
        let toml = r#"
[[points]]
device_id = "dev-b"
point_id = "b1"
protocol = "mc"
address = "D100"
frequency_ms = 2000

[[points]]
device_id = "dev-a"
point_id = "a1"
protocol = "modbus-tcp"
address = "127.0.0.1:502"
frequency_ms = 100

[[points]]
device_id = "dev-b"
point_id = "b1"
protocol = "mc"
address = "D100"
frequency_ms = 3000
"#;
        let config = GatewayConfig::parse(toml).expect("parse");
        let groups = build_groups(&config);
        let names: Vec<&str> = groups.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, vec!["dev-b", "dev-a"], "first-seen order");
        assert_eq!(groups[0].interval, Duration::from_millis(2000));
        assert_eq!(groups[0].point_ids, vec!["b1".to_string()], "deduped");
    }

    // ---- 集成波次接线：完整性自检（task 50） ----

    /// QA 接线：debug 构建下完整性自检跳过（恒 `None`，不产生假受限）。
    /// release 路径（manifest 缺失 → Unavailable → 受限）无法在本测试进程
    /// 真实触发（release 测试 exe 无 manifest），由 `expected_exe_hash_manifest` 覆盖解析层。
    #[cfg(debug_assertions)]
    #[test]
    fn startup_integrity_check_is_skipped_in_debug_builds() {
        assert!(
            startup_integrity_check().is_none(),
            "debug 构建必须跳过完整性自检"
        );
    }

    /// QA 接线：expected 哈希 manifest 解析 —— 64 hex 放行、垃圾 / 缺失拒绝
    /// （`None` → release 路径按 Unavailable 进入受限模式，fail-safe）。
    #[test]
    fn expected_exe_hash_manifest_parsing() {
        let dir = tempfile::tempdir().expect("tempdir");

        let good = dir.path().join("good.integrity-manifest");
        std::fs::write(&good, format!("{}{}", "a".repeat(64), "\n")).expect("write good");
        assert!(
            read_expected_exe_hash(&good).is_some(),
            "64 hex（含尾随空白）必须可解析"
        );

        let garbage = dir.path().join("garbage.integrity-manifest");
        std::fs::write(&garbage, "zz-not-hex").expect("write garbage");
        assert!(
            read_expected_exe_hash(&garbage).is_none(),
            "非 hex 必须拒绝"
        );

        let short = dir.path().join("short.integrity-manifest");
        std::fs::write(&short, "aabb").expect("write short");
        assert!(read_expected_exe_hash(&short).is_none(), "长度不足必须拒绝");

        assert!(
            read_expected_exe_hash(&dir.path().join("missing.integrity-manifest")).is_none(),
            "manifest 缺失必须返回 None"
        );
    }

    // ---- 集成波次接线：OTA 启动判定（task 35） ----

    /// QA 接线 Happy：注入「真实 `OtaManager` + 空内存 store」的判定钩子，
    /// 无 pending 槽位 → `NoPending` 零写操作（no-op），bootstrap 照常 Running，
    /// 钩子恰被调用一次。
    #[tokio::test(start_paused = true)]
    async fn ota_boot_check_no_pending_is_noop_and_reaches_running() {
        use crate::ota::{InMemoryOtaStore, OtaManager};
        use ed25519_dalek::SigningKey;

        let dir = tempfile::tempdir().expect("tempdir");
        let manager = Arc::new(tokio::sync::Mutex::new(OtaManager::new(
            Arc::new(InMemoryOtaStore::default()),
            SigningKey::from_bytes(&[0x1au8; 32]).verifying_key(),
            1,
        )));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let hook: OtaBootCheckHook = {
            let manager = Arc::clone(&manager);
            let calls = Arc::clone(&calls);
            Arc::new(move || {
                calls.fetch_add(1, Ordering::SeqCst);
                let manager = Arc::clone(&manager);
                Box::pin(async move {
                    let decision = manager.lock().await.boot_commit_or_rollback(false).await?;
                    assert_eq!(
                        decision,
                        OtaBootDecision::NoPending,
                        "无 meta 无 pending 必须 NoPending"
                    );
                    Ok(decision)
                }) as OtaBootFuture
            })
        };

        let shared = DaemonShared::new();
        let builder = BootstrapBuilder::new(write_test_config(&dir))
            .with_shared(shared.clone())
            .without_signal_handlers()
            .with_ota_boot_check(hook);

        let handle = wait_for_state(
            tokio::spawn(builder.run()),
            &shared,
            LifecycleState::Running,
        )
        .await;
        shared.request_shutdown();
        handle.await.expect("run task joins").expect("run ok");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "启动判定钩子必须恰被调用一次"
        );
    }

    /// QA 接线：判定结果为回滚（`RolledBackTo`）→ bootstrap 打显式 [WARN]
    /// （此处以标记观测「结果被消费」），**不阻断启动**（照常 Running）。
    #[tokio::test(start_paused = true)]
    async fn ota_boot_check_rollback_warns_but_boots() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

        let hook: OtaBootCheckHook = {
            let log = Arc::clone(&log);
            Arc::new(move || {
                if let Ok(mut guard) = log.lock() {
                    guard.push("rollback-decision".to_string());
                }
                Box::pin(async {
                    Ok(OtaBootDecision::RolledBackTo { version: 1 }) as DaemonResult<_>
                }) as OtaBootFuture
            })
        };

        let shared = DaemonShared::new();
        let builder = BootstrapBuilder::new(write_test_config(&dir))
            .with_shared(shared.clone())
            .without_signal_handlers()
            .with_ota_boot_check(hook);

        let handle = wait_for_state(
            tokio::spawn(builder.run()),
            &shared,
            LifecycleState::Running,
        )
        .await;
        assert_eq!(
            log.lock().expect("log lock").as_slice(),
            ["rollback-decision"],
            "回滚判定必须在 Running 前被消费一次"
        );
        shared.request_shutdown();
        let result = handle.await.expect("run task joins").expect("run ok");
        assert_eq!(
            result.state(),
            LifecycleState::Stopped,
            "回滚不阻断优雅停机"
        );
    }

    /// QA 接线：未注入 OTA 判定钩子 → no-op warn，启动流程不受影响。
    #[tokio::test(start_paused = true)]
    async fn ota_boot_check_defaults_to_noop_without_hook() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shared = DaemonShared::new();
        let builder = BootstrapBuilder::new(write_test_config(&dir))
            .with_shared(shared.clone())
            .without_signal_handlers();
        let handle = wait_for_state(
            tokio::spawn(builder.run()),
            &shared,
            LifecycleState::Running,
        )
        .await;
        shared.request_shutdown();
        handle.await.expect("run task joins").expect("run ok");
    }

    // ---- 集成波次接线：北向运行期（task 19 + task 54 最后一跳） ----

    /// 记录型审计出口：累计收到的事件条数（接线证明用）。
    #[derive(Default)]
    struct CountingAuditSink {
        emitted: Mutex<usize>,
    }

    impl crate::north::mqtt::AuditSink for CountingAuditSink {
        fn emit(&self, events: Vec<crate::backpressure::BackpressureAudit>) {
            if let Ok(mut guard) = self.emitted.lock() {
                *guard = guard.saturating_add(events.len());
            }
        }
    }

    /// 在临时目录打开一个真实 [`OfflineQueue`]（落盘降级 + 补发源）。
    fn temp_queue(dir: &tempfile::TempDir) -> Arc<crate::offline_queue::OfflineQueue> {
        let cfg = crate::offline_queue::QueueConfig::new(dir.path().join("queue.db"), "gw-test")
            .expect("queue cfg");
        Arc::new(
            crate::offline_queue::OfflineQueue::open(
                cfg,
                Arc::new(crate::offline_queue::SystemClock),
            )
            .expect("open queue"),
        )
    }

    /// 接线证明（**摘掉接线即失败**）：注入 `NorthRuntimeConfig` 后，
    /// 出口注册表挂到 `DaemonShared::north_runtime`、驱动任务真的在跑
    /// （`poll_cycles` 增长）、停机时驱动任务被全部中止。
    #[tokio::test(start_paused = true)]
    async fn north_runtime_wires_outlets_and_driver_task_runs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = temp_queue(&dir);
        let shared = DaemonShared::new();
        let builder = BootstrapBuilder::new(write_test_config(&dir))
            .with_shared(shared.clone())
            .without_signal_handlers()
            .with_north_runtime(NorthRuntimeConfig::new(
                Arc::clone(&queue),
                Arc::new(crate::offline_queue::SystemClock),
                Arc::new(CountingAuditSink::default()),
            ));

        let handle = wait_for_state(
            tokio::spawn(builder.run()),
            &shared,
            LifecycleState::Running,
        )
        .await;

        // 接线点：句柄可经 DaemonShared 拿到（采集侧运行期投递入口）。
        let runtime = shared
            .north_runtime()
            .expect("north runtime must be wired into DaemonShared");
        assert_eq!(runtime.outlet_count(), 1, "one outlet from [[outlets]]");
        assert_eq!(runtime.outlet_names(), vec!["north-1"]);
        assert!(
            runtime.outlet("north-1").is_some(),
            "outlet reachable by name"
        );
        assert!(
            matches!(
                runtime.submit("north-1", 1, vec![1, 2, 3]),
                Ok(crate::backpressure::PushOutcome::Admitted)
            ),
            "fresh send queue must admit the batch (non-blocking ingest is wired)"
        );

        // 驱动任务真的在跑：推进虚拟时间越过一个 tick（首拍已消费）。
        let mut cycles = runtime.stats().poll_cycles;
        for _ in 0..40 {
            tokio::time::advance(Duration::from_millis(50)).await;
            tokio::task::yield_now().await;
            let now = runtime.stats().poll_cycles;
            if now > cycles {
                cycles = now;
                break;
            }
        }
        assert!(
            cycles > 0,
            "driver task must execute poll cycles (wiring removed → stays 0)"
        );

        shared.request_shutdown();
        handle.await.expect("run task joins").expect("run ok");
        assert_eq!(
            runtime.running_tasks(),
            0,
            "shutdown step 3 must abort all north driver tasks"
        );
    }

    /// 反向接线证明：未注入 `NorthRuntimeConfig` → `DaemonShared` 无北向句柄
    /// （与上一个测试互为对照，共同锁定「接线存在」这一事实）。
    #[tokio::test(start_paused = true)]
    async fn north_runtime_absent_without_injection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shared = DaemonShared::new();
        let builder = BootstrapBuilder::new(write_test_config(&dir))
            .with_shared(shared.clone())
            .without_signal_handlers();
        let handle = wait_for_state(
            tokio::spawn(builder.run()),
            &shared,
            LifecycleState::Running,
        )
        .await;
        assert!(
            shared.north_runtime().is_none(),
            "no injection → no runtime (wiring is what creates it)"
        );
        shared.request_shutdown();
        handle.await.expect("run task joins").expect("run ok");
    }

    // ---- 集成波次接线：授权运行期（task 22/23/24） ----

    /// 恒开放行的测试闸门（`AuthSigner` 需要；本测试不签名）。
    struct BootstrapAlwaysLicensed;
    impl LicenseGate for BootstrapAlwaysLicensed {
        fn can_sign(&self) -> bool {
            true
        }
    }

    /// 构造测试用机器码指纹（64 hex）。
    fn bootstrap_test_mid() -> String {
        let identity = MachineIdentity::new(
            vec![Box::new(StaticAnchor::new(
                "boot-anchor",
                Some("gw-license-001"),
            ))],
            1,
            FingerprintKey::from_bytes(b"TEST_ONLY_bootstrap_fp_key".to_vec())
                .expect("non-empty fp key"),
        );
        identity
            .get_machine_fingerprint()
            .expect("quorum ok: 1 usable anchor")
    }

    /// 构造一个未接线 transport 的授权客户端（纯本地 C 档）。
    fn bootstrap_test_license_client() -> Arc<LicensingClient> {
        let mid = bootstrap_test_mid();
        let signer = Arc::new(
            AuthSigner::new(
                Arc::new(StaticKeyProvider::new([0x1au8; 32])),
                Arc::new(BootstrapAlwaysLicensed),
                mid.clone(),
            )
            .expect("mid is 64-hex"),
        );
        let cfg = LicensingClientConfig::new("https://licensing.test/v1", 24, 10, 30)
            .expect("non-empty url");
        Arc::new(LicensingClient::new(cfg, signer, mid))
    }

    /// 接线证明：注入 `LicenseRuntimeConfig` 后句柄挂到 `DaemonShared::license_runtime`，
    /// 启动即步进（纯本地 → `Trial`），且停机时循环被通知退出（run 正常返回）。
    #[tokio::test(start_paused = true)]
    async fn license_runtime_wires_and_shuts_down() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shared = DaemonShared::new();
        let rt_cfg =
            LicenseRuntimeConfig::new(bootstrap_test_license_client(), dir.path().join("data"));

        let builder = BootstrapBuilder::new(write_test_config(&dir))
            .with_shared(shared.clone())
            .without_signal_handlers()
            .with_license_runtime(rt_cfg);

        let handle = wait_for_state(
            tokio::spawn(builder.run()),
            &shared,
            LifecycleState::Running,
        )
        .await;

        let runtime = shared
            .license_runtime()
            .expect("license runtime must be wired into DaemonShared");

        // 驱动循环启动即步进一次（首拍立即触发）；让独立任务推进。
        for _ in 0..50 {
            if runtime.is_initialized() {
                break;
            }
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_millis(1)).await;
        }
        assert!(
            runtime.is_initialized(),
            "startup phase must have run at least once"
        );
        assert_eq!(runtime.state().name(), "Trial", "pure-local start ⇒ Trial");
        assert!(
            runtime.north_forward_allowed(),
            "Trial must allow northbound forward"
        );

        shared.request_shutdown();
        let result = handle.await.expect("run task joins").expect("run ok");
        assert_eq!(
            result.state(),
            LifecycleState::Stopped,
            "license runtime shutdown must not block graceful stop"
        );
    }

    /// 反向接线证明：未注入 `LicenseRuntimeConfig` → `DaemonShared::license_runtime()`
    /// 为 `None`，其余装配行为不变（与 north 的缺省范式一致）。
    #[tokio::test(start_paused = true)]
    async fn license_runtime_absent_without_injection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shared = DaemonShared::new();
        let builder = BootstrapBuilder::new(write_test_config(&dir))
            .with_shared(shared.clone())
            .without_signal_handlers();
        let handle = wait_for_state(
            tokio::spawn(builder.run()),
            &shared,
            LifecycleState::Running,
        )
        .await;
        assert!(
            shared.license_runtime().is_none(),
            "no injection → no license runtime"
        );
        shared.request_shutdown();
        let result = handle.await.expect("run task joins").expect("run ok");
        assert_eq!(result.state(), LifecycleState::Stopped);
    }

    // ---- 免费版配额闸门（启动装配期，task 22/23/24 门控接线） ----

    /// 构造 N 台 modbus 设备（间隔合规）+ 一个北向出口的测试配置。
    fn write_device_config(dir: &tempfile::TempDir, device_count: usize) -> PathBuf {
        let mut toml = String::from(
            "[gateway]\ngateway_id = \"gw-quota\"\n\n[[outlets]]\nname = \"north-1\"\n\
             broker = \"mqtt://127.0.0.1:1883\"\n\n",
        );
        for i in 0..device_count {
            toml.push_str(&format!(
                "[[points]]\ndevice_id = \"dev-{i:02}\"\npoint_id = \"p1\"\n\
                 protocol = \"modbus-tcp\"\naddress = \"127.0.0.1:502\"\n\
                 frequency_ms = 1000\n\n"
            ));
        }
        let path = dir.path().join("config.toml");
        std::fs::write(&path, toml).expect("write config");
        path
    }

    /// 制造 Degraded 初始状态：在 `data_dir` 路径上放一个**文件**，试用标记
    /// 无法落盘 → fail-closed 降级（不 panic）。
    fn block_data_dir(dir: &tempfile::TempDir) -> PathBuf {
        let data_file = dir.path().join("data");
        std::fs::write(&data_file, b"not a directory").expect("write blocker file");
        data_file
    }

    /// QA 闸门（装配期拒绝）：Degraded + 9 台设备（超免费配额）→ run 返回
    /// `ConfigError`，消息含实际数 / 恢复路径（fail-closed、可解释）。
    #[tokio::test(start_paused = true)]
    async fn bootstrap_degraded_rejects_over_quota_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = write_device_config(&dir, 9);
        let data_dir = block_data_dir(&dir);

        let shared = DaemonShared::new();
        let shared_probe = shared.clone();
        let builder = BootstrapBuilder::new(config_path)
            .with_shared(shared)
            .without_signal_handlers()
            .with_license_runtime(LicenseRuntimeConfig::new(
                bootstrap_test_license_client(),
                data_dir,
            ));

        // 装配期即拒绝：不会进入 Running（await 直接拿到 Err）。
        let err = builder
            .run()
            .await
            .expect_err("over-quota must be rejected");
        assert_eq!(err.error_code(), crate::error::ERR_CONFIG, "ConfigError 域");
        let msg = err.to_string();
        assert!(msg.contains("free-edition"), "reason prefix: {msg}");
        assert!(msg.contains('9'), "actual device count: {msg}");
        assert!(msg.contains("device_id"), "field name: {msg}");
        assert!(
            msg.contains("activate"),
            "recovery path must be included: {msg}"
        );

        // 收口：步进循环优雅退出（不残留到下一个测试）。
        if let Some(rt) = shared_probe.license_runtime() {
            rt.shutdown();
        }
    }

    /// QA 闸门（降级 ≠ 停用）：Degraded + 合规配置（1 台 / 1000ms / modbus）
    /// → 照常 Running、本地采集调度器在跑；北向出口被授权门控接管
    ///（`gated_cycles` 增长 = 跳过发送语义生效），停机正常。
    #[tokio::test(start_paused = true)]
    async fn bootstrap_degraded_compliant_config_runs_and_gates_north() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = write_device_config(&dir, 1);
        let data_dir = block_data_dir(&dir);

        let shared = DaemonShared::new();
        let queue = temp_queue(&dir);
        let builder = BootstrapBuilder::new(config_path)
            .with_shared(shared.clone())
            .without_signal_handlers()
            .with_license_runtime(LicenseRuntimeConfig::new(
                bootstrap_test_license_client(),
                data_dir,
            ))
            .with_north_runtime(NorthRuntimeConfig::new(
                queue,
                Arc::new(crate::offline_queue::SystemClock),
                Arc::new(CountingAuditSink::default()),
            ));

        let handle = wait_for_state(
            tokio::spawn(builder.run()),
            &shared,
            LifecycleState::Running,
        )
        .await;

        // 初始状态为 Degraded（fail-closed），但 daemon 照常运行（降级 ≠ 停用）。
        let license = shared
            .license_runtime()
            .expect("license runtime must be wired");
        assert_eq!(license.state().name(), "Degraded");
        assert!(license.state().allows_local_capture());

        // 北向门控生效：gated_cycles 增长（出口在运行、发送被跳过）。
        let north = shared.north_runtime().expect("north runtime must be wired");
        let mut gated = 0u64;
        for _ in 0..200 {
            tokio::time::advance(Duration::from_millis(50)).await;
            tokio::task::yield_now().await;
            gated = north.stats().gated_cycles;
            if gated > 0 {
                break;
            }
        }
        assert!(gated > 0, "north driver must be gated while degraded");

        shared.request_shutdown();
        let result = handle.await.expect("run task joins").expect("run ok");
        assert_eq!(result.state(), LifecycleState::Stopped);
    }
}
