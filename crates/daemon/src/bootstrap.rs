//! task 51 — daemon 进程装配与生命周期（bootstrap / 优雅停机 / 看门狗）。
//!
//! ## 职责边界
//! - **做**：[`DaemonShared`] 生命周期共享态（state watch / Utc 纳秒心跳 / 配置快照）；
//!   [`BootstrapBuilder`] 链式装配（配置热重载 → 调度器 → 北向启动钩子 → Running）；
//!   优雅停机（调度器停 → 离线队列 flush → 北向停 → Stopped，总宽限期 30s）；
//!   看门狗（基于 `last_heartbeat` 陈旧度判定 + 单次重启钩子）。
//! - **不做**：真实北向连接（经 `north_starter` / `north_stopper` 钩子解耦，task 19+ 集成）；
//!   OfflineQueue 实例装配（task 17/18 集成后把 flush 钩子替换为真实调用）。
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
//! 3. 离线队列 flush 为钩子（默认 no-op + TODO）：bootstrap 尚不持有 `OfflineQueue`
//!    实例（其构造需要 db 路径与 gateway_id 装配决策），集成任务接入后替换。
//! 4. `run()` 支持外部注入 [`DaemonShared`]（[`BootstrapBuilder::with_shared`]）：
//!    管理 API（mgmt）与北向都需要在运行期持有同一句柄，故不能只在 run 结束时返回。

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;
use tracing::{error, info, warn};

use crate::config::{ConfigShared, ConfigHotReloader, GatewayConfig};
use crate::error::DaemonResult;
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
        self.inner
            .shutdown_timed_out
            .store(true, Ordering::Relaxed);
    }

    /// 停机是否超宽限期被中止。
    pub fn shutdown_timed_out(&self) -> bool {
        self.inner.shutdown_timed_out.load(Ordering::Relaxed)
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

    /// 装配并运行 daemon 直至优雅停机完成，返回共享状态句柄。
    ///
    /// 装配顺序：共享状态 → 配置热重载 → 调度器 → 北向钩子 → Running；
    /// 停机顺序：调度器 → flush → north → Stopped。
    ///
    /// # Errors
    /// 配置初始加载 / watcher 初始化失败 → [`DaemonError`]（此时未产生任何后台任务）。
    pub async fn run(self) -> DaemonResult<DaemonShared> {
        // ① 初始化共享状态。
        let shared = self.shared.clone().unwrap_or_default();
        shared.set_state(LifecycleState::Starting);
        info!("bootstrap: daemon starting");

        // ② 配置热重载：初始加载失败直接返回错误（绝不静默空配置）。
        let (reloader, config_shared) = ConfigHotReloader::spawn(&self.config_path)?;
        shared.set_config(config_shared.clone());
        shared.notify_config_reload(config_shared.version());

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

        // ④ 北向启动钩子。
        match &self.north_starter {
            Some(hook) => hook(shared.clone()).await,
            None => warn!(
                "bootstrap: north_starter not configured; northbound not started (no-op hook)"
            ),
        }

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

/// 信号等待：ctrl_c +（unix）SIGTERM + 编程式停机请求，任一触发即返回。
async fn wait_for_signal(shared: &DaemonShared) {
    let mut shutdown_rx = shared.subscribe_shutdown();
    #[cfg(unix)]
    {
        let mut sigterm = match
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
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
    let steps = shutdown_sequence(builder, scheduler);
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
async fn shutdown_sequence(builder: &BootstrapBuilder, scheduler: Option<RunningScheduler>) {
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

    // ③ 北向停止钩子。
    match &builder.north_stopper {
        Some(hook) => hook().await,
        None => warn!("bootstrap: north_stopper not configured (no-op hook)"),
    }
    info!("bootstrap: shutdown step 3/3 northbound stopped");
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
            info!(version, "bootstrap: config reload propagated to DaemonShared");
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
        let entry = devices
            .entry(point.device_id.clone())
            .or_insert_with(|| {
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
        panic!("state did not reach {want:?} (current {:?})", shared.state());
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

        let handle =
            wait_for_state(tokio::spawn(builder.run()), &shared, LifecycleState::Running).await;

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

        let handle =
            wait_for_state(tokio::spawn(builder.run()), &shared, LifecycleState::Running).await;

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

        let handle =
            wait_for_state(tokio::spawn(builder.run()), &shared, LifecycleState::Running).await;

        // 注入「10s 前」的心跳 → 相对当前墙钟必然陈旧（> 200ms 阈值）。
        let stale_ns = utc_now_ns().saturating_sub(10_000_000_000);
        shared.touch_heartbeat(stale_ns);

        // 推进虚拟时间驱动看门狗 tick（首拍已消费，100ms 一拍）。
        for _ in 0..10 {
            if log.lock().expect("log lock").contains(&"restart".to_string()) {
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
}
