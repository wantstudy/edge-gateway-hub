//! `license` — 授权运行期编排（task 22/23/24/51 的接线层）。
//!
//! 本模块把此前**已实现但零接线**的授权子系统接到运行期：激活 → 24h 心跳 →
//! 离线宽限 7 天倒计时 → 试用 3 天到期降级 → 授权状态对采集 / 转发路径的暴露。
//! 它组合并驱动以下既有组件（**不重复实现其逻辑**）：
//! - [`crate::auth::client::LicensingClient`]：状态机内部件 + `/activate` / `/heartbeat`；
//! - [`crate::auth::trial`]：试用标记（机器码绑定 HMAC）判定 / 续期；
//! - [`crate::auth::clock::TrustedClock`]：可信时间 + 回拨检测；
//! - [`crate::auth::limits::FreeEditionLimits`]：免费版限制参数集。
//!
//! # 状态机语义（逐条）
//! - **启动**：读 `data_dir/trial.marker`（不存在 = 首次启动）→
//!   [`trial::evaluate`] 判定 → 有效未激活 ⇒ `Trial { days_left }`；
//!   标记损坏 / 到期 / 被篡改 ⇒ 直接 `Degraded`（**fail-closed**，绝不当作「新试用」）。
//! - **激活**：有 `activation_code` 且 `cloud_url` 非空 且当前未 `Licensed` ⇒
//!   [`LicensingClient::activate`]；成功 ⇒ `Licensed`，并 `trial::renew` 续期标记、落盘。
//! - **心跳**：距最后成功联网 ≥ `heartbeat_interval` ⇒ [`LicensingClient::heartbeat`]；
//!   成功 ⇒ 刷新「最后成功联网时刻」并回到 `Licensed`；失败 ⇒ **不得立即降级**，
//!   进入 / 继续 `Grace { days_left }` 倒计时（宽限默认 7 天）。
//! - **宽限**：`Grace` 剩余天数按「最后成功联网时刻 + 7 天」计算；归零 ⇒ `Degraded`。
//! - **试用到期**：`Trial.days_left` 归零 ⇒ `Degraded`（**降级 ≠ 停用**）。
//! - **恢复**：`Degraded` / 宽限内若心跳 / 激活成功 ⇒ 回到 `Licensed`。
//!
//! # 常驻不变式（不得违背）
//! 本模块**不持有采集开关**：任何状态（含 `Unlicensed` / `Degraded`）下，本模块
//! **只暴露判据**，绝不关闭本地采集。消费方（pipeline / scheduler）才是采集开关的
//! 持有者；本模块仅提供 [`LicenseRuntime::north_forward_allowed`]（北向转发判据）
//! 与 [`LicenseRuntime::limits`]（免费版限制口径）供下一波做门控。
//! 「降级 ≠ 停用」：`Degraded` 仅意味着停北向转发，本地采集必须继续。
//!
//! # 红线
//! - 客户端侧**不得**出现解绑 / 重置试用 / 废弃激活码入口（本模块不提供此类 API / 配置）。
//! - 授权判定必须在 Rust 侧；本模块**不**暴露任何「由 JS / WebView 传入并直接采信状态」
//!   的入口（构造器只接受 Rust 侧注入的类型化配置）。
//! - **不得硬编码**激活码 / 私钥：激活码经 [`LicenseRuntimeConfig::activation_code`]
//!   由装配方注入（生产从 env `IOTDAQ_ACTIVATION_CODE` 或后续 UI 注入）。
//! - 降级必须可解释：[`LicenseState::Degraded`] 的 `reason` 必须是可读原因，
//!   设备端界面据此展示「原因 + 恢复路径」。
//! - **红线 #13**：试用标记落盘位置**必须**来自 `gateway.data_dir`（容器内为宿主
//!   持久卷），**绝不**写容器可写层。`data_dir` 不可写时 **fail-closed 降级**
//!   （见 [`LicenseRuntime`] 文档），**不 panic**。
//!
//! # 可测试性
//! 步进函数 [`LicenseRuntime::step`] 与循环 [`LicenseRuntime::spawn`] 分离：
//! 测试**只调 `step()`**，靠注入的 [`LicenseRuntimeConfig::now_ms`] 推进虚拟时间，
//! 靠 mock [`crate::auth::client::LicenseTransport`] 返回响应——**无真实 sleep、不联网**。

use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tracing::{info, warn};

use crate::auth::client::{LicenseState, LicensingClient, GRACE_DAYS};
use crate::auth::clock::{LastSeenLoader, LastSeenSaver, TrustedClock};
use crate::auth::limits::FreeEditionLimits;
use crate::auth::trial::{self, HmacKey, TrialMarker, TrialVerdict};
use crate::error::DaemonResult;

// ---- 常量（集中声明，避免散落魔法数字） ----

/// 试用标记文件名（相对 `data_dir`；宿主持久卷内，红线 #13）。
pub const TRIAL_MARKER_FILE: &str = "trial.marker";

/// 默认心跳周期（秒，24h）；与 `config::LicensingSection::heartbeat_interval_secs` 默认一致。
pub const DEFAULT_HEARTBEAT_SECS: u64 = 86_400;

/// 默认编排器检查周期（秒）。
pub const DEFAULT_TICK: Duration = Duration::from_secs(30);

/// 一天毫秒数。
const MS_PER_DAY: u64 = 86_400_000;

/// 试用标记续期节流间隔（毫秒，6h）：避免每个 tick 都写盘。
const RENEW_INTERVAL_MS: u64 = 6 * 60 * 60 * 1000;

/// 当前墙钟（Unix 毫秒）；系统时钟早于纪元时按 0 处理（不 panic）。
fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

// ---- 配置 ----

/// 授权运行期装配输入（**不新增配置段**：`cloud_url` / `heartbeat_interval` 从既有
/// `gateway.licensing` 读取，`data_dir` 从 `gateway.data_dir` 读取；其余由装配方注入）。
pub struct LicenseRuntimeConfig {
    /// 云授权客户端（已注入 transport 与 device_signer）。
    pub client: Arc<LicensingClient>,
    /// 云授权服务地址；`None` = 纯本地 C 档（不联网，只跑试用 / 降级判定）。
    pub cloud_url: Option<String>,
    /// 心跳周期（来自 `gateway.licensing.heartbeat_interval_secs`）。
    pub heartbeat_interval: Duration,
    /// 编排器检查周期（测试可设为很小）。
    pub tick: Duration,
    /// 试用标记落盘目录（**必须**来自 `gateway.data_dir`，宿主持久卷）。
    pub data_dir: PathBuf,
    /// 激活码（**不得硬编码**；装配方从 env `IOTDAQ_ACTIVATION_CODE` 或 UI 注入）。
    pub activation_code: Option<String>,
    /// 可注入时间源（Unix 毫秒）；生产传墙钟，测试传虚拟时钟。
    pub now_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl LicenseRuntimeConfig {
    /// 以客户端与落盘目录构造（其余取默认：`cloud_url=None`、24h 心跳、30s tick、
    /// 无激活码、墙钟时间源）。
    #[must_use]
    pub fn new(client: Arc<LicensingClient>, data_dir: impl Into<PathBuf>) -> Self {
        Self {
            client,
            cloud_url: None,
            heartbeat_interval: Duration::from_secs(DEFAULT_HEARTBEAT_SECS),
            tick: DEFAULT_TICK,
            data_dir: data_dir.into(),
            activation_code: None,
            now_ms: Arc::new(wall_clock_ms),
        }
    }

    /// 覆盖云授权服务地址（非空即启用联网心跳 / 激活）。
    #[must_use]
    pub fn with_cloud_url(mut self, url: impl Into<String>) -> Self {
        self.cloud_url = Some(url.into());
        self
    }

    /// 覆盖心跳周期。
    #[must_use]
    pub fn with_heartbeat_interval(mut self, interval: Duration) -> Self {
        self.heartbeat_interval = interval;
        self
    }

    /// 覆盖编排器检查周期。
    #[must_use]
    pub fn with_tick(mut self, tick: Duration) -> Self {
        self.tick = tick;
        self
    }

    /// 覆盖激活码（装配方注入；**禁止硬编码进源码**）。
    #[must_use]
    pub fn with_activation_code(mut self, code: impl Into<String>) -> Self {
        self.activation_code = Some(code.into());
        self
    }

    /// 覆盖时间源（测试注入虚拟时钟）。
    #[must_use]
    pub fn with_now_ms(mut self, now_ms: Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
        self.now_ms = now_ms;
        self
    }
}

impl fmt::Debug for LicenseRuntimeConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LicenseRuntimeConfig")
            .field("cloud_url", &self.cloud_url)
            .field("heartbeat_interval", &self.heartbeat_interval)
            .field("tick", &self.tick)
            .field("data_dir", &self.data_dir)
            // 激活码是敏感串，Debug 输出脱敏（永不进日志）。
            .field(
                "activation_code",
                &self.activation_code.as_ref().map(|_| "<redacted>"),
            )
            .finish_non_exhaustive()
    }
}

// ---- 内部运行态 ----

/// 辅助运行态（受 `Mutex` 保护；授权状态本身由 `watch` 单独持有）。
#[derive(Debug, Default)]
struct AuxState {
    /// 启动阶段是否已执行（一次性）。
    initialized: bool,
    /// 最后成功联网时刻（Unix 毫秒；激活 / 心跳成功时刷新）。
    last_online_ms: Option<u64>,
    /// 最近一次激活尝试时刻（Unix 毫秒；节流用）。
    last_activation_attempt_ms: Option<u64>,
}

/// 授权运行期编排器。
///
/// ## 唯一真相源
/// 授权状态 [`LicenseState`] 由本模块（Rust 侧）持有，经 `watch` 广播：
/// [`LicenseRuntime::state`] / [`LicenseRuntime::subscribe`]。任何「能否北向转发」
/// 判定都只能读 [`LicenseRuntime::north_forward_allowed`]。
///
/// ## fail-closed（红线 #13）
/// 当 `data_dir` 不可写（容器只读可写层 / 权限不足）或试用标记不可读时，本模块
/// **降级**为 [`LicenseState::Degraded`]（`reason` 含可读原因），**绝不 panic**、
/// **绝不静默发放新试用**（否则只读 `data_dir` 就能反复刷免费试用）；若同时配置了
/// 云激活码，后续激活成功仍可恢复为 `Licensed`。
pub struct LicenseRuntime {
    /// 装配输入。
    cfg: LicenseRuntimeConfig,
    /// 授权状态广播（唯一真相源，单写多读）。
    state_tx: watch::Sender<LicenseState>,
    /// 辅助运行态。
    aux: Mutex<AuxState>,
    /// 试用回拨检测的 `last_seen` 持久锚点（供 `TrustedClock` 闭包注入；
    /// `Arc<Mutex<_>>` 使闭包满足 `'static + Send + Sync`）。
    trial_last_seen: Arc<Mutex<Option<u64>>>,
    /// 停机广播（`spawn` 的循环订阅它）。
    shutdown_tx: watch::Sender<bool>,
}

impl fmt::Debug for LicenseRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LicenseRuntime")
            .field("state", &self.state().name())
            .field("cloud_url", &self.cfg.cloud_url)
            .field("north_forward_allowed", &self.north_forward_allowed())
            .finish_non_exhaustive()
    }
}

impl LicenseRuntime {
    /// 以配置构造（初始状态 [`LicenseState::Unlicensed`]；首次 [`LicenseRuntime::step`] 完成启动）。
    #[must_use]
    pub fn new(cfg: LicenseRuntimeConfig) -> Self {
        let (state_tx, _) = watch::channel(LicenseState::Unlicensed);
        let (shutdown_tx, _) = watch::channel(false);
        Self {
            cfg,
            state_tx,
            aux: Mutex::new(AuxState::default()),
            trial_last_seen: Arc::new(Mutex::new(None)),
            shutdown_tx,
        }
    }

    /// 只读配置。
    #[must_use]
    pub fn config(&self) -> &LicenseRuntimeConfig {
        &self.cfg
    }

    /// 当前授权状态快照（**授权判定的唯一真相源**）。
    #[must_use]
    pub fn state(&self) -> LicenseState {
        self.state_tx.borrow().clone()
    }

    /// 订阅授权状态变化。
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<LicenseState> {
        self.state_tx.subscribe()
    }

    /// **本波只暴露该判据**（不改 pipeline）：是否允许北向转发。
    ///
    /// 仅 `Trial` / `Licensed` / `Grace` 为 `true`；`Unlicensed` / `Degraded` 为 `false`。
    #[must_use]
    pub fn north_forward_allowed(&self) -> bool {
        self.state().allows_northbound_forward()
    }

    /// 免费版限制参数集（`Degraded` / 免费版口径；供下一波做设备数 / 采集间隔门控）。
    #[must_use]
    pub fn limits(&self) -> FreeEditionLimits {
        FreeEditionLimits::new()
    }

    /// 试用标记文件路径（`data_dir/trial.marker`；宿主持久卷，红线 #13）。
    #[must_use]
    pub fn trial_marker_path(&self) -> PathBuf {
        self.cfg.data_dir.join(TRIAL_MARKER_FILE)
    }

    /// 最后成功联网时刻（Unix 毫秒；诊断用）。
    #[must_use]
    pub fn last_online_ms(&self) -> Option<u64> {
        self.aux_lock().last_online_ms
    }

    /// 启动阶段是否已执行。
    #[must_use]
    pub fn is_initialized(&self) -> bool {
        self.aux_lock().initialized
    }

    // ---- 步进 / 循环 ----

    /// 一次纯步进：启动阶段（试点 / 激活）+ 心跳到期判定 + 宽限倒计时 +
    /// 降级判定 + 试用标记续期。**不做 sleep、不联网之外不做阻塞**。
    ///
    /// 测试直接调本方法；由 [`LicenseRuntime::spawn`] 的循环按 `tick` 反复调用。
    ///
    /// # Errors
    /// 本方法对可恢复错误（网络失败 / 标记不可写）**内部降级或保留状态**，
    /// 目前恒返回 `Ok`（保留 `DaemonResult` 以承载后续不可恢复错误的传播）。
    pub async fn step(&self) -> DaemonResult<()> {
        let now = (self.cfg.now_ms)();
        // ① 启动阶段（一次性）：试用标记判定（+ 首次落盘）。
        self.initialize(now);
        // ② 激活恢复：有激活码且联网配置且未持租约时尝试激活。
        self.maybe_activate(now).await;
        // ③ 心跳到期判定：持租约时按周期心跳，失败进入 / 继续宽限。
        self.maybe_heartbeat(now).await;
        // ④ 倒计时 / 降级判定 + 试用标记续期。
        self.recompute(now);
        Ok(())
    }

    /// 以 `tick` 为周期反复调 [`LicenseRuntime::step`] 的循环（优雅响应停机）。
    ///
    /// 首个 tick 立即触发（启动即步进，尽早生效）。停机经 [`LicenseRuntime::shutdown`]
    /// 广播，循环在下一拍 / 收到信号时退出。
    pub fn spawn(self: Arc<Self>) -> JoinHandle<()> {
        let rt = Arc::clone(&self);
        let mut shutdown = self.shutdown_tx.subscribe();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(rt.cfg.tick);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                if *shutdown.borrow_and_update() {
                    break;
                }
                tokio::select! {
                    _ = ticker.tick() => {}
                    _ = shutdown.changed() => break,
                }
                if let Err(err) = rt.step().await {
                    warn!(error = %err, "license runtime: step failed");
                }
            }
            info!("license runtime: step loop exited");
        })
    }

    /// 停机：广播退出信号（幂等、不阻塞宽限期）。
    pub fn shutdown(&self) {
        self.shutdown_tx.send_replace(true);
    }

    // ---- 状态机内部实现 ----

    /// 原子替换状态并记录迁移日志。
    fn transition(&self, next: LicenseState) {
        let prev = self.state();
        if prev.name() != next.name() {
            info!(
                from = prev.name(),
                to = next.name(),
                reason = ?next_reason(&next),
                "license runtime: state transition"
            );
        }
        self.state_tx.send_replace(next);
    }

    /// 启动阶段（一次性）：读试用标记 → 判定 → `Trial` / `Degraded`。
    fn initialize(&self, now: u64) {
        {
            let mut aux = self.aux_lock();
            if aux.initialized {
                return;
            }
            aux.initialized = true;
        }

        let key = match HmacKey::from_machine_fingerprint(self.cfg.client.machine_code()) {
            Ok(key) => key,
            Err(err) => {
                self.transition(LicenseState::Degraded {
                    reason: format!("trial key derivation failed: {err}"),
                });
                return;
            }
        };

        let marker_path = self.trial_marker_path();
        let raw = match std::fs::read_to_string(&marker_path) {
            Ok(text) => Some(text),
            // 不存在 ⇒ 首次启动（合法）。
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            // 其它读取失败（权限 / IO）⇒ fail-closed 降级，绝不当作新试用。
            Err(err) => {
                self.transition(LicenseState::Degraded {
                    reason: format!("trial marker unreadable ({}): {err}", marker_path.display()),
                });
                return;
            }
        };

        let clock = self.make_clock(now);
        match trial::evaluate(raw.as_deref(), &key, &clock) {
            TrialVerdict::Fresh {
                marker_json,
                expires_at_ms,
            } => match self.persist_marker(&marker_json) {
                Ok(()) => self.transition(LicenseState::Trial {
                    days_left: Self::days_left_ceil(expires_at_ms, now),
                }),
                Err(reason) => {
                    // data_dir 不可写 ⇒ fail-closed 降级（不 panic、不静默发新试用）。
                    warn!(%reason, "license runtime: cannot persist trial marker; failing closed");
                    self.transition(LicenseState::Degraded { reason });
                }
            },
            TrialVerdict::Active { expires_at_ms } => self.transition(LicenseState::Trial {
                days_left: Self::days_left_ceil(expires_at_ms, now),
            }),
            TrialVerdict::ExpiredDegrade => self.transition(LicenseState::Degraded {
                reason: "trial marker invalid or expired (fail-closed)".to_string(),
            }),
        }
    }

    /// 激活恢复：有激活码 + 云地址且未持租约时尝试激活（按 `heartbeat_interval` 节流）。
    async fn maybe_activate(&self, now: u64) {
        if !self.cloud_enabled() {
            return;
        }
        let code = match self.cfg.activation_code.as_deref() {
            Some(code) if !code.trim().is_empty() => code.trim().to_string(),
            _ => return,
        };
        let current = self.state();
        // 已持租约（Licensed / Grace）不再激活。
        if matches!(
            current,
            LicenseState::Licensed { .. } | LicenseState::Grace { .. }
        ) {
            return;
        }
        // 节流：上次尝试距现在不足一个心跳周期则跳过（避免每拍打网络）。
        let last_attempt = self.aux_lock().last_activation_attempt_ms;
        if let Some(last) = last_attempt {
            if now.saturating_sub(last) < self.heartbeat_interval_ms() {
                return;
            }
        }
        self.set_last_activation_attempt(now);

        match self.cfg.client.activate(&code).await {
            Ok(LicenseState::Licensed { lease }) => {
                self.set_last_online(now);
                self.transition(LicenseState::Licensed { lease });
                // 激活成功：续期试用标记（强制，不节流）并落盘。
                self.renew_marker_from_disk(now, 0);
            }
            Ok(other) => self.transition(other),
            Err(err) => {
                // 激活失败**不降级**（可能是瞬时网络）：保留当前状态，等待下次节流窗口重试。
                warn!(error = %err, "license runtime: activation attempt failed (will retry)");
            }
        }
    }

    /// 心跳到期判定：持租约时按周期心跳，失败进入 / 继续宽限。
    async fn maybe_heartbeat(&self, now: u64) {
        if !self.cloud_enabled() {
            return;
        }
        let current = self.state();
        if !matches!(
            current,
            LicenseState::Licensed { .. } | LicenseState::Grace { .. }
        ) {
            return;
        }
        let due = match self.last_online_ms() {
            Some(last) => now.saturating_sub(last) >= self.heartbeat_interval_ms(),
            None => true,
        };
        if !due {
            return;
        }

        let cursor = self.cfg.client.last_cursor();
        match self.cfg.client.heartbeat(cursor).await {
            Ok(_) => {
                // 心跳成功 = 已联网：刷新最后联网时刻，回到 Licensed（租约取自客户端真相源）。
                self.set_last_online(now);
                self.transition(self.cfg.client.current_state());
            }
            Err(err) => {
                // **B 档红线**：断网不得立即降级 ⇒ 进入 / 继续离线宽限。
                warn!(error = %err, "license runtime: heartbeat failed; entering/continuing offline grace");
                self.enter_or_continue_grace(now);
            }
        }
    }

    /// 进入 / 继续 `Grace`（剩余天数按「最后成功联网时刻 + 7 天」计算；归零降级）。
    fn enter_or_continue_grace(&self, now: u64) {
        let lease = match self.cfg.client.current_state() {
            LicenseState::Licensed { lease } | LicenseState::Grace { lease, .. } => lease,
            // 无租约（理论上不会发生在心跳失败路径）：保状态，不臆造。
            _ => return,
        };
        let anchor = self.last_online_ms().unwrap_or(now);
        let days_left = Self::grace_days_left(anchor, now);
        if days_left <= 0 {
            self.transition(LicenseState::Degraded {
                reason: "offline grace expired".to_string(),
            });
        } else {
            self.transition(LicenseState::Grace { lease, days_left });
        }
    }

    /// 倒计时 / 降级判定（`Trial` 到期、`Grace` 耗尽）+ 试用标记续期。
    fn recompute(&self, now: u64) {
        match self.state() {
            LicenseState::Trial { .. } => self.recompute_trial(now),
            LicenseState::Grace { lease, .. } => {
                let anchor = self.last_online_ms().unwrap_or(now);
                let days_left = Self::grace_days_left(anchor, now);
                if days_left <= 0 {
                    self.transition(LicenseState::Degraded {
                        reason: "offline grace expired".to_string(),
                    });
                } else {
                    self.transition(LicenseState::Grace { lease, days_left });
                }
            }
            _ => {}
        }
    }

    /// `Trial` 倒计时：重算剩余天数 / 检测到期 / 检测篡改 / 续期标记。
    fn recompute_trial(&self, now: u64) {
        let key = match HmacKey::from_machine_fingerprint(self.cfg.client.machine_code()) {
            Ok(key) => key,
            Err(err) => {
                self.transition(LicenseState::Degraded {
                    reason: format!("trial key derivation failed: {err}"),
                });
                return;
            }
        };
        let marker_path = self.trial_marker_path();
        let raw = match std::fs::read_to_string(&marker_path) {
            Ok(text) => text,
            // 标记被删 ⇒ fail-closed（绝不重置为 3 天）。
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                self.transition(LicenseState::Degraded {
                    reason: "trial marker missing (fail-closed)".to_string(),
                });
                return;
            }
            Err(err) => {
                self.transition(LicenseState::Degraded {
                    reason: format!("trial marker unreadable ({}): {err}", marker_path.display()),
                });
                return;
            }
        };

        let clock = self.make_clock(now);
        match trial::evaluate(Some(&raw), &key, &clock) {
            TrialVerdict::Active { expires_at_ms } => {
                let days_left = Self::days_left_ceil(expires_at_ms, now);
                if days_left <= 0 {
                    self.transition(LicenseState::Degraded {
                        reason: "trial expired".to_string(),
                    });
                } else {
                    self.renew_marker_from_disk(now, RENEW_INTERVAL_MS);
                    self.transition(LicenseState::Trial { days_left });
                }
            }
            // 标记损坏 / 到期 / 被篡改 ⇒ fail-closed 降级（绝不重置为 3 天）。
            TrialVerdict::ExpiredDegrade => self.transition(LicenseState::Degraded {
                reason: "trial expired or marker invalid (fail-closed)".to_string(),
            }),
            // 传入 Some 标记时不会返回 Fresh；防御式保持现状。
            TrialVerdict::Fresh { .. } => {}
        }
    }

    /// 写试用标记（首建）。
    ///
    /// # Errors
    /// `data_dir` 不可创建 / 标记不可写 → 返回可读原因字符串（调用方据此 fail-closed 降级）。
    fn persist_marker(&self, json: &str) -> Result<(), String> {
        let dir = &self.cfg.data_dir;
        std::fs::create_dir_all(dir)
            .map_err(|err| format!("trial data dir not writable ({}): {err}", dir.display()))?;
        let path = self.trial_marker_path();
        std::fs::write(&path, json)
            .map_err(|err| format!("trial marker not writable ({}): {err}", path.display()))
    }

    /// 从磁盘读标记并续期（`min_advance_ms` 为最小推进量节流；0 = 强制）。
    ///
    /// 任何读取 / 解析失败都**静默跳过**（best-effort，不改变授权状态）。
    fn renew_marker_from_disk(&self, now: u64, min_advance_ms: u64) {
        let raw = match std::fs::read_to_string(self.trial_marker_path()) {
            Ok(raw) => raw,
            Err(_) => return,
        };
        let marker: TrialMarker = match serde_json::from_str(&raw) {
            Ok(marker) => marker,
            Err(_) => return,
        };
        if now.saturating_sub(marker.last_seen_ms) < min_advance_ms {
            return;
        }
        let key = match HmacKey::from_machine_fingerprint(self.cfg.client.machine_code()) {
            Ok(key) => key,
            Err(_) => return,
        };
        let mut last_seen = marker.last_seen_ms;
        let renewed = trial::renew(&marker, &mut last_seen, &key, now);
        if let Err(err) = std::fs::write(self.trial_marker_path(), renewed) {
            warn!(error = %err, "license runtime: trial marker renew failed (continuing)");
        }
    }

    /// 构造绑定本运行态 `last_seen` 存储的 [`TrustedClock`]（含回拨检测）。
    fn make_clock(&self, wall_now_ms: u64) -> TrustedClock {
        let load: LastSeenLoader = {
            let store = Arc::clone(&self.trial_last_seen);
            Box::new(move || *store.lock().unwrap_or_else(|err| err.into_inner()))
        };
        let save: LastSeenSaver = {
            let store = Arc::clone(&self.trial_last_seen);
            Box::new(move |value: u64| {
                *store.lock().unwrap_or_else(|err| err.into_inner()) = Some(value);
            })
        };
        TrustedClock::new(wall_now_ms, load, save)
    }

    // ---- 辅助 ----

    /// 独立测试友好：`Trial` 剩余天数（向上取整；已过期 / 到点为 0）。
    fn days_left_ceil(expires_at_ms: u64, now_ms: u64) -> i64 {
        if expires_at_ms <= now_ms {
            return 0;
        }
        let remaining = expires_at_ms - now_ms;
        let days = remaining.div_ceil(MS_PER_DAY);
        i64::try_from(days).unwrap_or(i64::MAX)
    }

    /// `Grace` 剩余天数（「最后成功联网时刻 + 7 天」；耗尽为 ≤ 0）。
    fn grace_days_left(anchor_ms: u64, now_ms: u64) -> i64 {
        let elapsed_days = now_ms.saturating_sub(anchor_ms) / MS_PER_DAY;
        GRACE_DAYS.saturating_sub(i64::try_from(elapsed_days).unwrap_or(i64::MAX))
    }

    /// 心跳周期毫秒。
    fn heartbeat_interval_ms(&self) -> u64 {
        u64::try_from(self.cfg.heartbeat_interval.as_millis()).unwrap_or(u64::MAX)
    }

    /// 是否启用联网（`cloud_url` 非空）。
    fn cloud_enabled(&self) -> bool {
        matches!(self.cfg.cloud_url.as_deref(), Some(url) if !url.trim().is_empty())
    }

    /// 中毒恢复的锁获取（**绝不 panic**）。
    fn aux_lock(&self) -> MutexGuard<'_, AuxState> {
        self.aux.lock().unwrap_or_else(|err| err.into_inner())
    }

    /// 记录最后成功联网时刻。
    fn set_last_online(&self, now: u64) {
        self.aux_lock().last_online_ms = Some(now);
    }

    /// 记录一次激活尝试时刻（节流）。
    fn set_last_activation_attempt(&self, now: u64) {
        self.aux_lock().last_activation_attempt_ms = Some(now);
    }
}

/// `Degraded` 状态的降级原因（供迁移日志；其它状态为 `None`）。
fn next_reason(state: &LicenseState) -> Option<&str> {
    match state {
        LicenseState::Degraded { reason } => Some(reason.as_str()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::client::{LeaseToken, VerifyMode};

    /// 构造一个占位租约（字段全 `pub`，仅供本模块纯逻辑测试）。
    fn sample_lease() -> LeaseToken {
        LeaseToken {
            raw: "k.p.s".to_string(),
            lease_id: "lease-0001".to_string(),
            device_id: "dev-0001".to_string(),
            kid: "kid-a".to_string(),
            tier: "standard".to_string(),
            verify_mode: VerifyMode::B,
            valid_until: 4_102_444_800,
        }
    }

    /// QA：`north_forward_allowed` 的状态矩阵（5 个状态逐项断言）。
    ///
    /// `Trial` / `Licensed` / `Grace` → `true`；`Unlicensed` / `Degraded` → `false`。
    #[test]
    fn north_forward_matrix_over_five_states() {
        let lease = sample_lease();
        let cases: [(LicenseState, bool); 5] = [
            (LicenseState::Unlicensed, false),
            (LicenseState::Trial { days_left: 3 }, true),
            (
                LicenseState::Licensed {
                    lease: lease.clone(),
                },
                true,
            ),
            (
                LicenseState::Grace {
                    lease: lease.clone(),
                    days_left: 5,
                },
                true,
            ),
            (
                LicenseState::Degraded {
                    reason: "trial expired".to_string(),
                },
                false,
            ),
        ];
        for (state, expected) in cases {
            assert_eq!(
                state.allows_northbound_forward(),
                expected,
                "state `{}` north-forward verdict mismatch",
                state.name()
            );
            // 降级 ≠ 停用：任何状态下本地采集判据恒为真。
            assert!(
                state.allows_local_capture(),
                "state `{}` must always allow local capture",
                state.name()
            );
        }
    }

    /// QA：`Trial` 剩余天数向上取整；到点 / 过期归 0。
    #[test]
    fn days_left_ceil_rounds_up_and_floors_expired_to_zero() {
        let t0 = 1_700_000_000_000u64;
        // 新鲜试用：now + 72h ⇒ 3 天。
        assert_eq!(LicenseRuntime::days_left_ceil(t0 + 3 * MS_PER_DAY, t0), 3);
        // 差 1ms 到期 ⇒ 仍算 1 天（向上取整）。
        assert_eq!(LicenseRuntime::days_left_ceil(t0 + 1, t0), 1);
        // 到点 ⇒ 0。
        assert_eq!(LicenseRuntime::days_left_ceil(t0, t0), 0);
        // 已过期 ⇒ 0。
        assert_eq!(LicenseRuntime::days_left_ceil(t0, t0 + MS_PER_DAY), 0);
    }

    /// QA：`Grace` 剩余天数按「最后联网 + 7 天」；边界与耗尽。
    #[test]
    fn grace_days_left_boundaries() {
        let anchor = 1_700_000_000_000u64;
        assert_eq!(LicenseRuntime::grace_days_left(anchor, anchor), 7);
        assert_eq!(
            LicenseRuntime::grace_days_left(anchor, anchor + 6 * MS_PER_DAY),
            1
        );
        // 恰好 7 天 ⇒ 0（耗尽）。
        assert_eq!(
            LicenseRuntime::grace_days_left(anchor, anchor + 7 * MS_PER_DAY),
            0
        );
        // 超过 7 天 ⇒ 负数。
        assert!(LicenseRuntime::grace_days_left(anchor, anchor + 8 * MS_PER_DAY) < 0);
    }
}
