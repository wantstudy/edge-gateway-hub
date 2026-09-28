//! 组轮询调度器（计划 task 16，Wave 2b）。
//!
//! ## 职责边界
//! - **做**：按 group（组）独立轮询；每组独立频率 + 独立 tokio task；组内点位批量下发；
//!   单组失败的隔离与统计。
//! - **不做**：动态频率调整（计划明确后置 task）；点位解码 / 单位换算（task 15 `pipeline`）；
//!   规则引擎（task 20/37）；北向转发（task 19+）。
//!
//! ## 关键设计决策
//!
//! 1. **一组一 task 一 interval**：每组持有独立 [`tokio::time::Interval`]，组间频率互不干扰。
//!    组 A(100ms) 与组 B(1s) 并行 1s → A 采 10 次、B 采 1 次（见单测
//!    `happy_path_independent_group_frequencies`）。
//! 2. **首次采集推迟一个周期**：用 `interval_at(now + interval, interval)` 而非
//!    `interval()`，使 `已采集次数 == 已过时间 / 周期` 严格成立，便于 QA 确定性断言；
//!    同时避免启动瞬间所有组同时爆发式采集。
//! 3. **`MissedTickBehavior::Delay`**：驱动阻塞后不补采（Burst 会瞬时连发，放大南向压力），
//!    下一拍按周期顺延。
//! 4. **组内批量读取**：一次 `poll` 传入整组 `point_ids`，由 [`PollHandler`] 实现内部合并为
//!    一次驱动批量请求，减少连接 / 握手开销。
//! 5. **错误隔离**：单组 `poll` 返回 `Err` 只记统计 + `warn!` 日志，本组后续调度与其他组
//!    均不受影响，绝不 panic、绝不退出任务循环。
//! 6. **轮询动作抽象为 trait**：[`PollHandler`] 使单测可注入计数器 / 故障注入器，
//!    无需真实驱动（配合 `tokio::time::pause()` + `advance()` 零真实等待）。
//! 7. **可选单组超时**：`GroupConfig::poll_timeout` 用 `tokio::time::timeout` 包裹单次轮询，
//!    超时按 `ProtocolError` 计入错误统计（错误码 1000），不拖垮调度。
//! 8. **轮询动作取 `&self`**（非计划建议的 `&mut self`）：调度器需把 handler 放进 `Arc` 供
//!    多个组任务共享，`&mut self` 会强制外层再加互斥锁；内部可变性由实现方自行选择
//!    （原子计数器 / `Mutex`），见「偏差说明」。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::task::JoinHandle;
use tokio::time::{interval_at, Instant, MissedTickBehavior};
use tracing::{info, warn};

use crate::backpressure::AcquisitionGovernor;
use crate::config::GatewayConfig;
use crate::error::{DaemonError, DaemonResult};
use crate::pipeline::RawSample;

// ---- 配置 ----

/// 单个采集组的配置（构建后只读，**不支持动态频率调整** —— 计划明确后置）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupConfig {
    /// 组名（调度器内唯一，用于日志与统计索引）。
    pub name: String,
    /// 轮询周期（> 0；如 100ms / 1s / 10s）。
    pub interval: Duration,
    /// 组内点位标识列表（非空、元素非空、互不重复；一次 `poll` 整组下发）。
    pub point_ids: Vec<String>,
    /// 单次轮询超时（可选；`None` = 不设超时，由驱动自身超时控制）。
    pub poll_timeout: Option<Duration>,
}

impl GroupConfig {
    /// 构造组配置并校验；非法即 [`DaemonError::ConfigError`]（错误码 2000）。
    ///
    /// # Errors
    /// 组名为空 / 周期为 0 / 点位列表为空 / 点位名为空 / 点位重复时返回 `ConfigError`。
    pub fn new(
        name: impl Into<String>,
        interval: Duration,
        point_ids: Vec<String>,
    ) -> DaemonResult<Self> {
        let config = Self {
            name: name.into(),
            interval,
            point_ids,
            poll_timeout: None,
        };
        config.validate()?;
        Ok(config)
    }

    /// 设置单次轮询超时（`> 0`），返回 `Self` 以支持链式构造。
    ///
    /// # Errors
    /// 超时为 0 时返回 [`DaemonError::ConfigError`]。
    pub fn with_timeout(mut self, timeout: Duration) -> DaemonResult<Self> {
        if timeout.is_zero() {
            return Err(DaemonError::ConfigError(format!(
                "group config {}: poll_timeout must be greater than zero",
                self.name
            )));
        }
        self.poll_timeout = Some(timeout);
        Ok(self)
    }

    /// 配置自检（与 [`GroupConfig::new`] 同一套规则，供外部复用）。
    pub fn validate(&self) -> DaemonResult<()> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err(DaemonError::ConfigError(
                "group config: name must not be empty".to_string(),
            ));
        }
        if self.interval.is_zero() {
            return Err(DaemonError::ConfigError(format!(
                "group config {}: interval must be greater than zero",
                self.name
            )));
        }
        if self.point_ids.is_empty() {
            return Err(DaemonError::ConfigError(format!(
                "group config {}: point_ids must contain at least one point",
                self.name
            )));
        }
        let mut seen: HashSet<&str> = HashSet::with_capacity(self.point_ids.len());
        for (index, point_id) in self.point_ids.iter().enumerate() {
            if point_id.trim().is_empty() {
                return Err(DaemonError::ConfigError(format!(
                    "group config {}: point_ids[{index}] must not be empty",
                    self.name
                )));
            }
            if !seen.insert(point_id.as_str()) {
                return Err(DaemonError::ConfigError(format!(
                    "group config {}: duplicated point id {point_id:?} at index {index}",
                    self.name
                )));
            }
        }
        Ok(())
    }
}

// ---- 轮询动作抽象 ----

/// 组内批量轮询动作（一次调用 = 该组一轮采集）。
///
/// 实现方（通常是「驱动适配器」）负责把整组 `point_ids` 合并为尽量少的南向请求；
/// 返回本轮成功采集的**样本**（D-14 数据面接线：样本按点位一一对应，含解码后
/// 数值 / 质量码 / 时间戳），调度器只取 `len()` 记统计——样本本身由
/// [`crate::dataplane::NorthDataPlane`] 消费（管线变换 → 北向投递）。
#[async_trait]
pub trait PollHandler: Send + Sync {
    /// 执行 `group` 的一轮批量采集，返回成功采集的样本。
    ///
    /// 返回 `Err` 时调度器记录错误统计并继续下一拍，**不会**终止该组调度
    /// （错误 = 整组无样本；个别点位解码失败由实现方跳过并告警，不整体失败）。
    async fn poll(&self, group: &str, point_ids: &[String]) -> DaemonResult<Vec<RawSample>>;

    /// 配置热重载：让本 handler 重新推导它的**设备 / 点位计划表**（默认 no-op）。
    ///
    /// 绝大多数实现是无状态采集动作，默认实现为空即可。持有「设备计划表」这类
    /// 派生状态的实现（如 [`crate::southbound::DevicePollHandler`]）**必须重写**，
    /// 否则重载后会出现半新半旧：调度器侧已经起了新组，采集动作侧仍按旧设备表
    /// 派活，新组的每一拍都报 `unknown device group`，新点位永远采不到数。
    ///
    /// 实现契约（调用方依赖这些性质做错误隔离，不要破坏）：
    /// - **只改内存计划表**，绝不中止调用方（调度器）的任务与连接；
    /// - 已删除设备的残留连接必须摘掉（不留僵尸南向连接）；
    /// - 同名设备的既有连接要保留（避免热重载时重建连接造成采集断流）。
    async fn refresh_devices(&self, _config: &GatewayConfig) {}
}

// ---- 运行时统计 ----

/// 单组运行时统计（原子计数；可在调度任务与断言方之间安全共享）。
#[derive(Debug, Default)]
pub struct GroupStats {
    /// 轮询次数（含失败尝试）。
    polls: AtomicU64,
    /// 累计成功采集样本数（`poll` 返回值累加）。
    samples: AtomicU64,
    /// 累计失败次数。
    errors: AtomicU64,
    /// 最近一次错误消息（用于日志 / 诊断展示）。
    last_error: Mutex<Option<String>>,
}

impl GroupStats {
    /// 轮询次数（含失败尝试）。
    pub fn polls(&self) -> u64 {
        self.polls.load(Ordering::Relaxed)
    }

    /// 累计成功采集样本数。
    pub fn samples(&self) -> u64 {
        self.samples.load(Ordering::Relaxed)
    }

    /// 累计失败次数。
    pub fn errors(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }

    /// 最近一次错误消息（无错误时为 `None`）。
    pub fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().cloned())
    }

    /// 记录一轮成功采集。
    fn record_success(&self, samples: usize) {
        self.polls.fetch_add(1, Ordering::Relaxed);
        self.samples.fetch_add(samples as u64, Ordering::Relaxed);
    }

    /// 记录一轮失败采集。
    fn record_failure(&self, message: &str) {
        self.polls.fetch_add(1, Ordering::Relaxed);
        self.errors.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut guard) = self.last_error.lock() {
            *guard = Some(message.to_string());
        }
    }
}

/// 共享统计表：`组名 -> 该组统计`。
pub type StatsMap = Arc<HashMap<String, Arc<GroupStats>>>;

// ---- 调度器 ----

/// 单组一次轮询的结果（供 [`GroupScheduler::poll_all`] 返回）。
#[derive(Debug)]
pub struct PollOutcome {
    /// 组名。
    pub group: String,
    /// 本轮结果：`Ok(样本数)` / `Err(错误)`（错误已同步计入统计）。
    pub result: DaemonResult<usize>,
}

/// 组轮询调度器：按组独立频率轮询，每组一个独立 tokio task。
#[derive(Debug)]
pub struct GroupScheduler<H: PollHandler> {
    /// 轮询动作（多组共享）。
    handler: Arc<H>,
    /// 组配置（构建期校验，运行期只读）。
    groups: Vec<GroupConfig>,
    /// 组名 -> 统计。
    stats: StatsMap,
}

impl<H: PollHandler> GroupScheduler<H> {
    /// 构建调度器并校验全部组配置。
    ///
    /// # Errors
    /// - 组列表为空 / 组名重复 / 任一 [`GroupConfig`] 非法 → `ConfigError`（2000）。
    pub fn new(handler: H, groups: Vec<GroupConfig>) -> DaemonResult<Self> {
        if groups.is_empty() {
            return Err(DaemonError::ConfigError(
                "scheduler: at least one group is required".to_string(),
            ));
        }

        let mut names: HashSet<&str> = HashSet::with_capacity(groups.len());
        let mut stats: HashMap<String, Arc<GroupStats>> = HashMap::with_capacity(groups.len());
        for group in &groups {
            group.validate()?;
            if !names.insert(group.name.as_str()) {
                return Err(DaemonError::ConfigError(format!(
                    "scheduler: duplicated group name {:?}",
                    group.name
                )));
            }
            stats.insert(group.name.clone(), Arc::new(GroupStats::default()));
        }

        Ok(Self {
            handler: Arc::new(handler),
            groups,
            stats: Arc::new(stats),
        })
    }

    /// 全部组配置（构建期校验过的只读视图）。
    pub fn groups(&self) -> &[GroupConfig] {
        &self.groups
    }

    /// 按名取组配置。
    pub fn group(&self, name: &str) -> Option<&GroupConfig> {
        self.groups.iter().find(|g| g.name == name)
    }

    /// 按名取该组统计句柄（与调度任务共享同一份计数）。
    pub fn stats(&self, name: &str) -> Option<Arc<GroupStats>> {
        self.stats.get(name).cloned()
    }

    /// 该组已轮询次数（未知组返回 `0`）。
    pub fn polls(&self, name: &str) -> u64 {
        self.stats(name).map_or(0, |s| s.polls())
    }

    /// 手动执行指定组的一轮轮询（统计同步累加）。
    ///
    /// 与后台任务共用同一条执行路径，供单测确定性断言与手动补采使用。
    ///
    /// # Errors
    /// - 未知组名 → `ConfigError`（2000）；
    /// - handler 失败 / 超时 → 原样返回（同步计入错误统计）。
    pub async fn poll_group(&self, name: &str) -> DaemonResult<usize> {
        let unknown = || DaemonError::ConfigError(format!("scheduler: unknown group {name:?}"));
        let config = self.group(name).ok_or_else(unknown)?;
        let stats = self.stats(name).ok_or_else(unknown)?;
        execute_poll(self.handler.as_ref(), config, &stats).await
    }

    /// 手动对每个组各执行一次轮询，返回逐组结果（顺序与 [`Self::groups`] 一致）。
    ///
    /// 任一组失败不影响其余组继续执行（错误隔离）。
    pub async fn poll_all(&self) -> Vec<PollOutcome> {
        let mut outcomes = Vec::with_capacity(self.groups.len());
        for config in &self.groups {
            let name = config.name.clone();
            let stats = self.stats.get(&name).cloned();
            let result = match stats {
                Some(stats) => execute_poll(self.handler.as_ref(), config, &stats).await,
                None => Err(DaemonError::ConfigError(format!(
                    "scheduler: missing stats for group {name:?}"
                ))),
            };
            outcomes.push(PollOutcome {
                group: name,
                result,
            });
        }
        outcomes
    }
}

/// 执行一轮轮询（含可选超时）+ 统计落账 + 错误隔离。
///
/// 手动路径（[`GroupScheduler::poll_group`] / [`GroupScheduler::poll_all`]）与后台任务
/// （[`spawn_group`]）共用此函数，保证两条路径行为与统计口径一致。
///
/// 参数取 `&dyn PollHandler`（而非泛型 `&H`）：启动后的 [`RunningScheduler`] 只持有
/// 类型擦除的 handler，重建组时才能复用同一份轮询动作重起任务。
async fn execute_poll(
    handler: &dyn PollHandler,
    config: &GroupConfig,
    stats: &GroupStats,
) -> DaemonResult<usize> {
    let future = handler.poll(&config.name, &config.point_ids);
    let polled: DaemonResult<Vec<RawSample>> = match config.poll_timeout {
        Some(timeout) => match tokio::time::timeout(timeout, future).await {
            Ok(result) => result,
            Err(_) => Err(DaemonError::ProtocolError(format!(
                "group {}: poll timeout after {}ms",
                config.name,
                timeout.as_millis()
            ))),
        },
        None => future.await,
    };

    match &polled {
        Ok(samples) => stats.record_success(samples.len()),
        Err(err) => {
            warn!("scheduler: group {} poll failed: {err}", config.name);
            stats.record_failure(&err.to_string());
        }
    }
    // 统计口径维持「样本数」；样本本体已由数据面（NorthDataPlane）消费。
    polled.map(|samples| samples.len())
}

impl<H: PollHandler + 'static> GroupScheduler<H> {
    /// 启动全部组的独立轮询任务（**必须在 tokio 运行时上下文中调用**）。
    ///
    /// 每组一个 task + 一个独立 interval；返回的 [`RunningScheduler`] 持有句柄与统计表，
    /// 被 drop 时自动中止全部任务。
    pub fn start(self) -> RunningScheduler {
        let Self {
            handler,
            groups,
            stats,
        } = self;
        let names: Vec<String> = groups.iter().map(|g| g.name.clone()).collect();
        let handles: Vec<JoinHandle<()>> = groups
            .into_iter()
            .map(|config| {
                let group_stats = stats
                    .get(&config.name)
                    .cloned()
                    .unwrap_or_else(|| Arc::new(GroupStats::default()));
                spawn_group(handler.clone(), config, group_stats, None, None, false)
            })
            .collect();
        RunningScheduler {
            handler,
            governor: None,
            rows_provider: None,
            names,
            handles,
            stats,
        }
    }

    /// 启动全部组的独立轮询任务，并接入**自适应节流调控器**（task 54 背压）。
    ///
    /// 与 [`Self::start`] 行为一致，但每组循环在每轮 `poll` 后调用
    /// [`AcquisitionGovernor::observe`] 观测内存队列行数（`rows_provider` 提供），
    /// 持续高位时逐级**放大轮询周期**（降采样），持续低位时恢复——带迟滞，不抖动。
    ///
    /// 该路径**默认不启用**：仅当显式调用本方法并传入调控器时才生效；不调用即与
    /// [`Self::start`] 完全等价（既有测试不受影响）。
    pub fn start_adaptive(
        self,
        governor: Arc<Mutex<AcquisitionGovernor>>,
        rows_provider: Arc<dyn Fn() -> usize + Send + Sync>,
    ) -> RunningScheduler {
        let Self {
            handler,
            groups,
            stats,
        } = self;
        let names: Vec<String> = groups.iter().map(|g| g.name.clone()).collect();
        let handles: Vec<JoinHandle<()>> = groups
            .into_iter()
            .map(|config| {
                let group_stats = stats
                    .get(&config.name)
                    .cloned()
                    .unwrap_or_else(|| Arc::new(GroupStats::default()));
                spawn_group(
                    handler.clone(),
                    config,
                    group_stats,
                    Some(governor.clone()),
                    Some(rows_provider.clone()),
                    false,
                )
            })
            .collect();
        RunningScheduler {
            handler,
            governor: Some(governor),
            rows_provider: Some(rows_provider),
            names,
            handles,
            stats,
        }
    }
}

/// 单组任务主循环：等拍 → 轮询 → 记账 → （可选）观测水位节流 → 继续（失败不退出）。
///
/// 参数取 `Arc<dyn PollHandler>`（而非泛型 `Arc<H>`）：启动后的 [`RunningScheduler`]
/// 只持有类型擦除的 handler，[`RunningScheduler::rebuild`] 才能复用同一份轮询动作
/// 重起任务，无需先把整座调度器拆了重建。
///
/// `resume`: 首拍是否**立刻**触发（[`RunningScheduler::rebuild`] 用）。冷启动必须
/// 按设计决策 2 推迟一个周期（避免瞬时爆发 + 便于 QA 精确断言）；但重建是「续跑」
/// ——旧任务刚被 abort，若同样推迟一个周期，被保留的设备就要空转一整个周期才恢复，
/// 南向连接虽在，数据仍会出现一段空档。故重建路径首拍立即采集。
fn spawn_group(
    handler: Arc<dyn PollHandler>,
    config: GroupConfig,
    stats: Arc<GroupStats>,
    governor: Option<Arc<Mutex<AcquisitionGovernor>>>,
    rows_provider: Option<Arc<dyn Fn() -> usize + Send + Sync>>,
    resume: bool,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // 首拍推迟一个周期（设计决策 2；`resume` 时立即）+ 阻塞后不补采（决策 3）。
        let first_tick_at = if resume {
            Instant::now()
        } else {
            Instant::now() + config.interval
        };
        let mut ticker = interval_at(first_tick_at, config.interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut current_interval = config.interval;
        info!(
            "scheduler: group {} started with interval {}ms, {} point(s)",
            config.name,
            config.interval.as_millis(),
            config.point_ids.len()
        );
        loop {
            // 等拍 → 轮询（返回值仅用于日志路径；失败已在 `execute_poll` 内记账）。
            ticker.tick().await;
            let _ = execute_poll(handler.as_ref(), &config, &stats).await;

            // 自适应节流（仅启用调控器时生效）：观测内存队列水位，持续高位则
            // 放大轮询周期（降采样），低位则恢复；迟滞防抖。
            if let (Some(gov), Some(provider)) = (&governor, &rows_provider) {
                if let Ok(mut guard) = gov.lock() {
                    let decision = guard.observe(provider());
                    let next = Duration::from_millis(decision.poll_interval_ms);
                    if next != current_interval {
                        current_interval = next;
                        // 重建 interval（首拍同样推迟一个周期，避免瞬间爆发）。
                        ticker = interval_at(Instant::now() + next, next);
                        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
                    }
                }
            }
        }
    })
}

/// 已启动的调度器句柄：持有各组任务句柄、共享统计表与重建所需的类型擦除上下文。
///
/// 不实现 `Debug`：`handler` 是 `dyn` 特质对象，无法派生；调试信息请用
/// [`Self::group_names`] 与 [`Self::polls`]。
pub struct RunningScheduler {
    /// 轮询动作（类型擦除，重建时复用）。
    handler: Arc<dyn PollHandler>,
    /// 自适应节流调控器（未启用时为 `None`）。
    governor: Option<Arc<Mutex<AcquisitionGovernor>>>,
    /// 内存队列水位观测器（未启用时为 `None`）。
    rows_provider: Option<Arc<dyn Fn() -> usize + Send + Sync>>,
    names: Vec<String>,
    handles: Vec<JoinHandle<()>>,
    stats: StatsMap,
}

impl RunningScheduler {
    /// 组名列表（启动顺序）。
    pub fn group_names(&self) -> &[String] {
        &self.names
    }

    /// 按名取该组统计句柄。
    pub fn stats(&self, name: &str) -> Option<Arc<GroupStats>> {
        self.stats.get(name).cloned()
    }

    /// 该组已轮询次数（未知组返回 `0`）。
    pub fn polls(&self, name: &str) -> u64 {
        self.stats(name).map_or(0, |s| s.polls())
    }

    /// 用新的组集合**就地重建**运行的任务（配置热重载用）。
    ///
    /// 语义与约束：
    /// 1. **先校验后动手**：组集合非法（空 / 组名重复 / 组内点位非法）直接返回
    ///    `Err`，`self` 保持原样——旧调度器继续跑（错误隔离，绝不半重建）。
    /// 2. **保留同名组的统计**：`polls` / `samples` / `errors` 沿用旧的
    ///    [`Arc<GroupStats>`]，UI 的采集次数与成功率不会归零闪烁。
    /// 3. **先 abort 旧 handle，再换上新的**：旧任务在 abort 后不再轮询，
    ///    被删组的僵尸采集被就地掐掉；新句柄在成功 spawn 后才写入字段。
    /// 4. **被删组的统计条目一并摘掉**：`stats()` / `polls()` 对已删组返回 `None`，
    ///    与「该组不再运行」保持一致，避免 UI 读到永不前进的僵尸计数。
    /// 5. **续跑语义**：重建时各任务首拍**立即**采集（[`Self::spawn_group`] 的
    ///    `resume`），不等满新周期——被保留的设备否则要空转一个周期才恢复，数据断档。
    /// 6. 失败时（只可能是第 1 步的校验）旧 `names` / `handles` / `stats` 全部保留。
    ///
    /// # Errors
    /// - 组集合为空 / 组名重复 / 任一 [`GroupConfig`] 非法 → `ConfigError`（2000）。
    pub fn rebuild(&mut self, groups: Vec<GroupConfig>) -> DaemonResult<()> {
        validate_groups(&groups)?;

        // 新统计表：同名组沿用旧计数（Arc 共享），新组从零开始。
        let mut rebuilt: HashMap<String, Arc<GroupStats>> = HashMap::with_capacity(groups.len());
        for group in &groups {
            let retained = self
                .stats
                .get(&group.name)
                .cloned()
                .unwrap_or_else(|| Arc::new(GroupStats::default()));
            rebuilt.insert(group.name.clone(), retained);
        }

        let names: Vec<String> = groups.iter().map(|g| g.name.clone()).collect();
        let mut handles: Vec<JoinHandle<()>> = Vec::with_capacity(groups.len());
        for config in groups {
            let group_stats = rebuilt
                .get(&config.name)
                .cloned()
                .unwrap_or_else(|| Arc::new(GroupStats::default()));
            handles.push(spawn_group(
                self.handler.clone(),
                config,
                group_stats,
                self.governor.clone(),
                self.rows_provider.clone(),
                true,
            ));
        }

        // 走到这里才动旧状态：旧 handle 先 abort（被删组立即停止采集），再整体替换；
        // 统计表换成新的一份（同名沿用、新组从零、已删组摘除）。
        self.abort();
        self.names = names;
        self.handles = handles;
        self.stats = Arc::new(rebuilt);
        info!(
            "scheduler: rebuilt {} group task(s): {:?}",
            self.handles.len(),
            self.names
        );
        Ok(())
    }

    /// 中止全部组任务（同步，不等待）。
    pub fn abort(&self) {
        for handle in &self.handles {
            handle.abort();
        }
    }

    /// 优雅停止：中止并等待全部组任务退出。
    pub async fn shutdown(mut self) {
        self.abort();
        // 本类型实现 `Drop`，不能直接移出字段：先取出句柄再逐个等待任务退出
        // （`Drop` 里的重复 abort 是幂等的）。
        for handle in std::mem::take(&mut self.handles) {
            let _ = handle.await;
        }
    }
}

/// 校验组集合：非空、组名互不重复、每组配置自检通过（与 [`GroupScheduler::new`] 同一套规则）。
///
/// # Errors
/// - 组集合为空 / 组名重复 / 任一 [`GroupConfig`] 非法 → `ConfigError`（2000）。
pub fn validate_groups(groups: &[GroupConfig]) -> DaemonResult<()> {
    if groups.is_empty() {
        return Err(DaemonError::ConfigError(
            "scheduler: at least one group is required".to_string(),
        ));
    }
    let mut names: HashSet<&str> = HashSet::with_capacity(groups.len());
    for group in groups {
        group.validate()?;
        if !names.insert(group.name.as_str()) {
            return Err(DaemonError::ConfigError(format!(
                "scheduler: duplicated group name {:?}",
                group.name
            )));
        }
    }
    Ok(())
}

/// 判断两组集合是否等价（**组身份** = 组名 + 组内点位 + 轮询周期 + 单次轮询超时）。
///
/// 配置热重载据此决定要不要 [`RunningScheduler::rebuild`]：等价就让既有任务继续跑，
/// 避免无谓的 abort / 重启造成采集空档（被删组仍会被差异检测捕获并 abort）。
pub fn groups_changed(a: &[GroupConfig], b: &[GroupConfig]) -> bool {
    if a.len() != b.len() {
        return true;
    }
    let mut left = a.to_vec();
    let mut right = b.to_vec();
    left.sort_by(|x, y| x.name.cmp(&y.name));
    right.sort_by(|x, y| x.name.cmp(&y.name));
    left.iter().zip(right.iter()).any(|(x, y)| {
        x.name != y.name
            || x.interval != y.interval
            || x.poll_timeout != y.poll_timeout
            || x.point_ids != y.point_ids
    })
}

impl Drop for RunningScheduler {
    fn drop(&mut self) {
        self.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backpressure::{AcquisitionGovernorConfig, AuditLog};
    use crate::error::ERR_CONFIG;
    use crate::offline_queue::{Clock, ManualClock};
    use std::sync::Mutex;

    // ---- 测试用 handler：计数器 + 故障注入 + 卡死注入 ----

    /// 批次记录：`(组名, 本次整组下发的点位列表)`。
    type BatchLog = Arc<Mutex<Vec<(String, Vec<String>)>>>;

    /// 测试 handler（可克隆探针：放入调度器后仍可在断言侧读计数与批次记录）。
    #[derive(Clone, Debug)]
    struct FakeHandler {
        /// 组名 -> 该组被轮询次数（handler 侧独立计数，避免只信调度器自报）。
        polls: Arc<HashMap<String, AtomicU64>>,
        /// 常驻失败的组（模拟驱动超时）。
        failing: HashSet<String>,
        /// 卡死的组：poll 内先睡 `duration` 再返回（模拟驱动无响应）。
        hanging: HashMap<String, Duration>,
        /// 每次调用收到的点位列表（校验「整组批量下发」）。
        batches: BatchLog,
    }

    impl FakeHandler {
        fn new(groups: &[(&str, &[&str])]) -> Self {
            let polls = groups
                .iter()
                .map(|(name, _)| ((*name).to_string(), AtomicU64::new(0)))
                .collect::<HashMap<_, _>>();
            Self {
                polls: Arc::new(polls),
                failing: HashSet::new(),
                hanging: HashMap::new(),
                batches: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn fail(mut self, group: &str) -> Self {
            self.failing.insert(group.to_string());
            self
        }

        fn hang(mut self, group: &str, duration: Duration) -> Self {
            self.hanging.insert(group.to_string(), duration);
            self
        }

        fn polls(&self, group: &str) -> u64 {
            self.polls
                .get(group)
                .map_or(0, |counter| counter.load(Ordering::Relaxed))
        }

        fn batches_for(&self, group: &str) -> Vec<Vec<String>> {
            let guard = self.batches.lock().expect("batch log lock");
            guard
                .iter()
                .filter(|(name, _)| name == group)
                .map(|(_, points)| points.clone())
                .collect()
        }
    }

    #[async_trait]
    impl PollHandler for FakeHandler {
        async fn poll(&self, group: &str, point_ids: &[String]) -> DaemonResult<Vec<RawSample>> {
            if let Ok(mut guard) = self.batches.lock() {
                guard.push((group.to_string(), point_ids.to_vec()));
            }
            if let Some(delay) = self.hanging.get(group) {
                tokio::time::sleep(*delay).await;
                return Ok(fake_samples(point_ids));
            }
            if self.failing.contains(group) {
                return Err(DaemonError::ProtocolError(format!(
                    "group {group}: driver timeout"
                )));
            }
            if let Some(counter) = self.polls.get(group) {
                counter.fetch_add(1, Ordering::Relaxed);
            }
            Ok(fake_samples(point_ids))
        }
    }

    /// 构造 `point_ids.len()` 个直通假样本（每点位值 = 1.0）。
    fn fake_samples(point_ids: &[String]) -> Vec<RawSample> {
        point_ids
            .iter()
            .map(|point_id| RawSample {
                source_id: point_id.clone(),
                value: 1.0,
                quality: protocol_proto::Quality::Good,
                device_ts_ns: None,
            })
            .collect()
    }

    /// 组配置快捷构造。
    fn group(name: &str, interval_ms: u64, points: &[&str]) -> GroupConfig {
        GroupConfig::new(
            name,
            Duration::from_millis(interval_ms),
            points.iter().map(|p| (*p).to_string()).collect(),
        )
        .expect("valid group config")
    }

    /// 一个**校验不过**的组配置：点位列表为空（`group()` 辅助函数会直接 panic，故单列）。
    fn empty_points() -> GroupConfig {
        GroupConfig {
            name: "A".to_string(),
            interval: Duration::from_millis(100),
            point_ids: Vec::new(),
            poll_timeout: None,
        }
    }

    /// 让刚 spawn 的组任务在 t=0 完成初始化（登记首个周期起点），避免首拍漂移。
    async fn settle() {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    /// 推进虚拟时间并让被唤醒的组任务跑完一轮。
    async fn advance(duration: Duration) {
        tokio::time::advance(duration).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
    }

    /// 推进 `steps × step` 虚拟时间，末尾多推进 1ms 跨过节拍边界
    /// （定时器在 `deadline == now` 时仍需一次调度轮询才就绪，跨过边界可消除歧义）。
    async fn advance_steps(step: Duration, steps: usize) {
        for _ in 0..steps {
            advance(step).await;
        }
        advance(Duration::from_millis(1)).await;
    }

    // ---- 配置校验 ----

    /// QA: 非法组配置在构建期返回 ConfigError（错误码 2000），不进入运行期。
    #[test]
    fn invalid_group_config_rejected_at_construction() {
        let cases: Vec<(&str, DaemonResult<GroupConfig>)> = vec![
            (
                "empty name",
                GroupConfig::new("", Duration::from_millis(100), vec!["p1".to_string()]),
            ),
            (
                "blank name",
                GroupConfig::new("   ", Duration::from_millis(100), vec!["p1".to_string()]),
            ),
            (
                "zero interval",
                GroupConfig::new("A", Duration::ZERO, vec!["p1".to_string()]),
            ),
            (
                "empty points",
                GroupConfig::new("A", Duration::from_millis(100), vec![]),
            ),
            (
                "blank point id",
                GroupConfig::new("A", Duration::from_millis(100), vec![" ".to_string()]),
            ),
            (
                "duplicated point id",
                GroupConfig::new(
                    "A",
                    Duration::from_millis(100),
                    vec!["p1".to_string(), "p1".to_string()],
                ),
            ),
            (
                "zero poll timeout",
                group("A", 100, &["p1"]).with_timeout(Duration::ZERO),
            ),
        ];
        for (label, result) in cases {
            let err = result.expect_err(label);
            assert_eq!(
                err.error_code(),
                ERR_CONFIG,
                "{label}: expected ConfigError, got {err}"
            );
        }

        // 合法配置应通过，且超时可链式设置。
        let ok = group("A", 100, &["p1", "p2"])
            .with_timeout(Duration::from_millis(50))
            .expect("valid");
        assert_eq!(ok.poll_timeout, Some(Duration::from_millis(50)));
        assert_eq!(ok.point_ids, vec!["p1".to_string(), "p2".to_string()]);
    }

    /// QA: 调度器构建期拦截空组列表与重名组。
    #[test]
    fn scheduler_rejects_empty_and_duplicated_groups() {
        let handler = FakeHandler::new(&[("A", &["p1"])]);
        let err = GroupScheduler::new(handler, vec![]).expect_err("empty group list");
        assert_eq!(err.error_code(), ERR_CONFIG);

        let handler = FakeHandler::new(&[("A", &["p1"])]);
        let err = GroupScheduler::new(
            handler,
            vec![group("A", 100, &["p1"]), group("A", 200, &["p2"])],
        )
        .expect_err("duplicated group name");
        assert_eq!(err.error_code(), ERR_CONFIG);
        assert!(err.to_string().contains("duplicated group name"));
    }

    // ---- QA Happy：组独立频率 ----

    /// QA Happy: 组 A(100ms) 与组 B(1s) 同时运行 → A 采集 10 次期间 B 采集 1 次。
    #[tokio::test(start_paused = true)]
    async fn happy_path_independent_group_frequencies() {
        let handler = FakeHandler::new(&[("A", &["a1", "a2"]), ("B", &["b1"])]);
        let probe = handler.clone();
        let scheduler = GroupScheduler::new(
            handler,
            vec![group("A", 100, &["a1", "a2"]), group("B", 1000, &["b1"])],
        )
        .expect("valid scheduler");
        let running = scheduler.start();
        settle().await;

        // 推进 1s 虚拟时间（分 10 拍，便于任务被轮询）。
        advance_steps(Duration::from_millis(100), 10).await;

        let stats_a = running.stats("A").expect("stats A");
        let stats_b = running.stats("B").expect("stats B");
        assert_eq!(stats_a.polls(), 10, "A@100ms must poll 10 times in 1s");
        assert_eq!(stats_b.polls(), 1, "B@1s must poll 1 time in 1s");
        assert_eq!(stats_a.errors(), 0);
        assert_eq!(stats_b.errors(), 0);
        // 组内点位批量读取：样本数 = 轮询次数 × 组内点位数。
        assert_eq!(stats_a.samples(), 20, "A has 2 points per poll");
        assert_eq!(stats_b.samples(), 1, "B has 1 point per poll");
        // handler 侧独立计数与调度器自报一致；每个批次都是整组点位（批量读取）。
        assert_eq!(probe.polls("A"), 10);
        assert_eq!(probe.polls("B"), 1);
        assert_eq!(probe.batches_for("A").len(), 10);
        assert_eq!(
            probe.batches_for("A").first().expect("first batch"),
            &vec!["a1".to_string(), "a2".to_string()]
        );

        // 继续推进 1s：A 再采 10 次（累计 20），B 再采 1 次（累计 2）。
        advance_steps(Duration::from_millis(100), 10).await;
        assert_eq!(stats_a.polls(), 20);
        assert_eq!(stats_b.polls(), 2);

        running.shutdown().await;
    }

    // ---- QA Error：错误隔离 ----

    /// QA Error: 某组驱动超时 → 只影响本组统计，不影响其他组调度。
    #[tokio::test(start_paused = true)]
    async fn error_isolation_failed_group_does_not_affect_others() {
        let handler = FakeHandler::new(&[("fast", &["f1"]), ("broken", &["x1"])]).fail("broken");
        let scheduler = GroupScheduler::new(
            handler,
            vec![group("fast", 100, &["f1"]), group("broken", 100, &["x1"])],
        )
        .expect("valid scheduler");
        let running = scheduler.start();
        settle().await;

        advance_steps(Duration::from_millis(100), 10).await;

        let fast = running.stats("fast").expect("stats fast");
        let broken = running.stats("broken").expect("stats broken");
        // 故障组：每拍都失败，但仍在被调度（未退出循环）。
        assert_eq!(broken.polls(), 10, "broken group must keep being scheduled");
        assert_eq!(broken.errors(), 10, "every poll fails");
        assert_eq!(broken.samples(), 0, "failed poll yields no sample");
        let last_error = broken.last_error().expect("last error recorded");
        assert!(last_error.contains("driver timeout"), "got {last_error}");
        // 正常组：完全不受影响。
        assert_eq!(fast.polls(), 10, "healthy group unaffected");
        assert_eq!(fast.errors(), 0);
        assert_eq!(fast.samples(), 10);

        running.shutdown().await;
    }

    /// 驱动卡死时由 `poll_timeout` 兜底，且慢组不拖垮快组（超时即释放，不阻塞后续调度）。
    #[tokio::test(start_paused = true)]
    async fn slow_group_timeout_does_not_block_fast_group() {
        let handler = FakeHandler::new(&[("fast", &["f1", "f2"]), ("slow", &["s1"])])
            .hang("slow", Duration::from_secs(30));
        let scheduler = GroupScheduler::new(
            handler,
            vec![
                group("fast", 100, &["f1", "f2"]),
                group("slow", 100, &["s1"])
                    .with_timeout(Duration::from_millis(50))
                    .expect("valid timeout"),
            ],
        )
        .expect("valid scheduler");
        let running = scheduler.start();
        settle().await;

        advance_steps(Duration::from_millis(100), 5).await;

        let fast = running.stats("fast").expect("stats fast");
        let slow = running.stats("slow").expect("stats slow");
        assert_eq!(fast.polls(), 5, "fast group keeps its 100ms cadence");
        assert_eq!(fast.errors(), 0);
        assert_eq!(fast.samples(), 10, "2 points × 5 polls");
        // 慢组：每次都被 50ms 超时打断，产出 0 样本；尝试次数不少于 2 次（至少继续被调度）。
        assert_eq!(slow.samples(), 0, "timeout yields no sample");
        assert!(
            slow.errors() >= 2,
            "slow group must be retried after timeout, got {}",
            slow.errors()
        );
        assert!(
            slow.errors() <= 5,
            "slow group must not burst-read, got {}",
            slow.errors()
        );
        let last_error = slow.last_error().expect("timeout recorded");
        assert!(last_error.contains("poll timeout"), "got {last_error}");
        // 快组与慢组的调度彼此独立：慢组超时的同时快组仍在采集。
        assert!(fast.polls() >= slow.errors());

        running.shutdown().await;
    }

    // ---- 手动驱动路径（确定性，不依赖时间推进） ----

    /// `poll_all`：每个组各跑一轮，整组点位一次性批量下发。
    #[tokio::test]
    async fn poll_all_runs_each_group_once_with_whole_group_points() {
        let handler = FakeHandler::new(&[("A", &["a1", "a2", "a3"]), ("B", &["b1"])]);
        let probe = handler.clone();
        let scheduler = GroupScheduler::new(
            handler,
            vec![
                group("A", 100, &["a1", "a2", "a3"]),
                group("B", 5000, &["b1"]),
            ],
        )
        .expect("valid scheduler");

        let outcomes = scheduler.poll_all().await;
        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0].group, "A");
        assert_eq!(outcomes[0].result.as_ref().expect("A ok"), &3);
        assert_eq!(outcomes[1].group, "B");
        assert_eq!(outcomes[1].result.as_ref().expect("B ok"), &1);

        assert_eq!(scheduler.polls("A"), 1);
        assert_eq!(scheduler.polls("B"), 1);
        let stats_a = scheduler.stats("A").expect("stats A");
        assert_eq!(stats_a.samples(), 3);
        assert_eq!(stats_a.errors(), 0);

        // 批量读取：一次调用收到整组 3 个点位，而非逐点 3 次。
        assert_eq!(probe.batches_for("A").len(), 1);
        assert_eq!(
            probe.batches_for("A").first().expect("one batch for A"),
            &vec!["a1".to_string(), "a2".to_string(), "a3".to_string()]
        );
        assert_eq!(
            probe.batches_for("B").first().expect("one batch for B"),
            &vec!["b1".to_string()]
        );
    }

    /// `poll_all`：某组失败不中断其余组，且错误落到该组统计。
    #[tokio::test]
    async fn poll_all_isolates_errors_across_groups() {
        let handler =
            FakeHandler::new(&[("ok1", &["p1"]), ("bad", &["p2"]), ("ok2", &["p3"])]).fail("bad");
        let scheduler = GroupScheduler::new(
            handler,
            vec![
                group("ok1", 100, &["p1"]),
                group("bad", 100, &["p2"]),
                group("ok2", 100, &["p3"]),
            ],
        )
        .expect("valid scheduler");

        let outcomes = scheduler.poll_all().await;
        assert_eq!(outcomes.len(), 3, "all groups attempted despite failure");
        assert!(outcomes[0].result.is_ok());
        assert!(outcomes[2].result.is_ok());
        let err = outcomes[1].result.as_ref().expect_err("bad group fails");
        assert_eq!(err.error_code(), crate::error::ERR_PROTOCOL);

        assert_eq!(scheduler.polls("bad"), 1);
        assert_eq!(scheduler.stats("bad").expect("stats bad").errors(), 1);
        assert_eq!(scheduler.stats("ok2").expect("stats ok2").samples(), 1);
    }

    /// 未知组名 → ConfigError，不影响其他组。
    #[tokio::test]
    async fn poll_unknown_group_returns_config_error() {
        let handler = FakeHandler::new(&[("A", &["a1"])]);
        let scheduler =
            GroupScheduler::new(handler, vec![group("A", 100, &["a1"])]).expect("valid scheduler");
        let err = scheduler
            .poll_group("nope")
            .await
            .expect_err("unknown group");
        assert_eq!(err.error_code(), ERR_CONFIG);
        assert!(err.to_string().contains("unknown group"));
        assert_eq!(scheduler.polls("A"), 0, "no side effect on known groups");
    }

    /// 组配置访问器与统计初始状态。
    #[test]
    fn accessors_expose_groups_and_zeroed_stats() {
        let handler = FakeHandler::new(&[("A", &["a1"]), ("B", &["b1"])]);
        let scheduler = GroupScheduler::new(
            handler,
            vec![group("A", 100, &["a1"]), group("B", 10_000, &["b1"])],
        )
        .expect("valid scheduler");

        assert_eq!(scheduler.groups().len(), 2);
        assert_eq!(
            scheduler.group("B").expect("group B").interval,
            Duration::from_secs(10)
        );
        assert!(scheduler.group("missing").is_none());
        assert_eq!(scheduler.polls("A"), 0);
        assert_eq!(scheduler.polls("missing"), 0);
        let stats = scheduler.stats("A").expect("stats A");
        assert_eq!(stats.polls(), 0);
        assert_eq!(stats.samples(), 0);
        assert_eq!(stats.errors(), 0);
        assert_eq!(stats.last_error(), None);
    }

    /// 三种频率（100ms / 1s / 10s）在同一调度器内共存且互不干扰。
    #[tokio::test(start_paused = true)]
    async fn three_frequencies_coexist_independently() {
        let handler = FakeHandler::new(&[("fast", &["f1"]), ("mid", &["m1"]), ("slow", &["s1"])]);
        let scheduler = GroupScheduler::new(
            handler,
            vec![
                group("fast", 100, &["f1"]),
                group("mid", 1_000, &["m1"]),
                group("slow", 10_000, &["s1"]),
            ],
        )
        .expect("valid scheduler");
        let running = scheduler.start();
        settle().await;

        advance_steps(Duration::from_millis(100), 10).await;
        assert_eq!(running.polls("fast"), 10);
        assert_eq!(running.polls("mid"), 1);
        assert_eq!(running.polls("slow"), 0);
        assert_eq!(running.group_names(), &["fast", "mid", "slow"]);

        running.shutdown().await;
    }

    /// `shutdown` 后任务停止：再推进时间不再产生采集。
    #[tokio::test(start_paused = true)]
    async fn shutdown_stops_all_group_tasks() {
        let handler = FakeHandler::new(&[("A", &["a1"])]);
        let scheduler =
            GroupScheduler::new(handler, vec![group("A", 100, &["a1"])]).expect("valid scheduler");
        let running = scheduler.start();
        settle().await;
        advance_steps(Duration::from_millis(100), 3).await;
        assert_eq!(running.polls("A"), 3);

        let stats = running.stats("A").expect("stats A");
        running.shutdown().await;
        advance_steps(Duration::from_millis(100), 5).await;
        assert_eq!(stats.polls(), 3, "no polling after shutdown");
    }

    // ---- 自适应节流（背压 Governor） ----

    /// 构造一个「高位即立即降采样」的调控器（基准 100ms，降采样后周期翻倍）。
    fn high_water_governor() -> Arc<Mutex<AcquisitionGovernor>> {
        let cfg = AcquisitionGovernorConfig {
            high_water_rows: 2,
            low_water_rows: 1,
            sustain_samples: 1,
            recover_samples: 1,
            reduce_step_permille: 500,
            min_factor_permille: 250,
            base_poll_interval_ms: 100,
        };
        let audit = Arc::new(AuditLog::new(64));
        let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(0));
        Arc::new(Mutex::new(AcquisitionGovernor::new(cfg, audit, clock)))
    }

    /// 水位持续高位 → 调度器被降采样（采集次数少于无调控基线）。
    #[tokio::test(start_paused = true)]
    async fn adaptive_governor_throttles_polling_when_queue_high() {
        let governor = high_water_governor();
        // 内存队列行数始终高于高水位（2）→ 应触发降采样。
        let rows_provider: Arc<dyn Fn() -> usize + Send + Sync> = Arc::new(|| 100);

        let handler = FakeHandler::new(&[("A", &["a1"])]);
        let scheduler =
            GroupScheduler::new(handler, vec![group("A", 100, &["a1"])]).expect("valid scheduler");
        let running = scheduler.start_adaptive(governor.clone(), rows_provider);
        settle().await;
        advance_steps(Duration::from_millis(100), 10).await;

        // 基准 100ms 在 1s 内本应采 10 次；持续高位**逐级**降采样（100→200→250ms）→ 采集次数明显减少。
        let polls = running.polls("A");
        assert!(polls < 10, "governor must reduce polling, got {polls}");
        assert!(polls > 0, "still some polling, got {polls}");
        assert_eq!(
            governor.lock().ok().map(|g| g.factor_permille()),
            Some(250),
            "factor reduced to the floor after sustained high water"
        );
        running.shutdown().await;
    }

    /// 水位持续低位 → 不降采样，行为与 `start()` 完全一致（既有的 10 次）。
    #[tokio::test(start_paused = true)]
    async fn adaptive_governor_does_not_throttle_when_queue_low() {
        let governor = high_water_governor();
        // 内存队列行数始终为 0（远低于低水位）→ 不应降采样。
        let rows_provider: Arc<dyn Fn() -> usize + Send + Sync> = Arc::new(|| 0);

        let handler = FakeHandler::new(&[("A", &["a1"])]);
        let scheduler =
            GroupScheduler::new(handler, vec![group("A", 100, &["a1"])]).expect("valid scheduler");
        let running = scheduler.start_adaptive(governor.clone(), rows_provider);
        settle().await;
        advance_steps(Duration::from_millis(100), 10).await;

        assert_eq!(running.polls("A"), 10, "low water → no throttling");
        assert_eq!(
            governor.lock().ok().map(|g| g.factor_permille()),
            Some(1_000),
            "factor stays at 1000 permille"
        );
        running.shutdown().await;
    }

    // ---- 热重载：就地重建采集组 ----

    /// 重建：新增组被拉起、被删组的任务被 abort、同名组的统计**不归零**。
    #[tokio::test(start_paused = true)]
    async fn rebuild_adds_removes_groups_and_retains_stats() {
        let handler = FakeHandler::new(&[("keep", &["k1"]), ("add", &["n1"])]);
        let probe = handler.clone();
        let scheduler = GroupScheduler::new(
            handler,
            vec![group("keep", 100, &["k1"]), group("drop", 100, &["d1"])],
        )
        .expect("valid scheduler");
        let mut running = scheduler.start();
        settle().await;

        advance_steps(Duration::from_millis(100), 3).await;
        let before_keep = running.polls("keep");
        assert!(before_keep > 0, "old group polled before rebuild");
        let kept_stats = running.stats("keep").expect("stats keep");

        // 「drop」消失、「keep」保留点位、「add」新建。
        running
            .rebuild(vec![
                group("keep", 100, &["k1"]),
                group("add", 100, &["n1"]),
            ])
            .expect("rebuild ok");
        settle().await;

        assert_eq!(
            running.group_names(),
            &["keep".to_string(), "add".to_string()],
            "removed group must not linger in the running set"
        );
        assert!(
            running.stats("drop").is_none(),
            "dropped group has no live task (no zombie polling)"
        );
        // 同名组沿用旧统计句柄（Arc 同一份 → 计数不归零）。
        assert!(
            Arc::ptr_eq(&kept_stats, &running.stats("keep").expect("stats keep")),
            "same-named group must retain the very same stats handle"
        );
        // 计数不归零（重建沿用同名组的统计句柄）：可能因「续跑首拍」多记 1 次。
        assert!(
            running.polls("keep") >= before_keep,
            "counters must not reset on rebuild"
        );
        // 重建走「续跑」语义：新组首拍立即采集，不必等满一个周期。
        assert!(
            running.polls("add") >= 1,
            "rebuilt group must poll immediately, got {}",
            running.polls("add")
        );

        // 被删组不再被轮询：推进时间后 handler 侧该组计数不前进。
        let before_drop_polls = probe.polls("drop");
        advance_steps(Duration::from_millis(100), 3).await;
        assert_eq!(
            probe.polls("drop"),
            before_drop_polls,
            "aborted group must stop polling (no zombie)"
        );
        // 新组与保留组都在跑。
        assert!(probe.polls("add") > 0, "new group is being polled");
        assert!(
            probe.polls("keep") > before_keep,
            "kept group keeps polling"
        );

        running.shutdown().await;
    }

    /// 重建不掐断保留组的采集：改周期（必触发 rebuild）后，保留组必须**立刻**再采，
    /// 而不是空转一整个周期——否则 `last_sample_at` 会断档（热重载抖动项）。
    #[tokio::test(start_paused = true)]
    async fn rebuild_resumes_kept_group_without_waiting_a_full_interval() {
        let handler = FakeHandler::new(&[("keep", &["k1"])]);
        let probe = handler.clone();
        let scheduler = GroupScheduler::new(handler, vec![group("keep", 100, &["k1"])])
            .expect("valid scheduler");
        let mut running = scheduler.start();
        settle().await;
        advance_steps(Duration::from_millis(100), 2).await;

        // 周期从 100ms 改成 60s：组身份变了 → 必须 rebuild；但采集不能停摆。
        let before = probe.polls("keep");
        assert!(before > 0, "group polled before rebuild");
        running
            .rebuild(vec![group("keep", 60_000, &["k1"])])
            .expect("rebuild ok");
        // 只推进 10ms（远小于 60s 新周期）：首拍若不「续跑立即」就不会有这次采集。
        advance(Duration::from_millis(10)).await;

        assert_eq!(
            probe.polls("keep"),
            before + 1,
            "kept group must poll once right after rebuild, not after a full interval"
        );
        assert_eq!(running.polls("keep"), before + 1, "stats keep counting up");

        running.shutdown().await;
    }

    /// 重建失败（空 / 组名重复 / 点位非法）保留旧调度器继续跑，绝不半重建。
    #[tokio::test(start_paused = true)]
    async fn rebuild_rejects_invalid_groups_and_keeps_old_scheduler_running() {
        let handler = FakeHandler::new(&[("A", &["a1"])]);
        let probe = handler.clone();
        let scheduler =
            GroupScheduler::new(handler, vec![group("A", 100, &["a1"])]).expect("valid scheduler");
        let mut running = scheduler.start();
        settle().await;
        advance_steps(Duration::from_millis(100), 2).await;
        let before = running.polls("A");

        let cases: Vec<(&str, Vec<GroupConfig>)> = vec![
            ("empty", vec![]),
            (
                "duplicated name",
                vec![group("A", 100, &["a1"]), group("A", 200, &["a2"])],
            ),
            ("empty point set", vec![empty_points()]),
        ];
        for (label, groups) in cases {
            let err = running.rebuild(groups).expect_err(label);
            assert_eq!(err.error_code(), ERR_CONFIG, "{label}: config error code");
            assert_eq!(
                running.group_names(),
                &["A".to_string()],
                "{label}: old groups untouched"
            );
            assert!(running.stats("A").is_some(), "{label}: old stats untouched");
        }

        // 旧任务仍在轮询：推进时间后计数继续前进（不是被悄悄停掉）。
        advance_steps(Duration::from_millis(100), 2).await;
        assert!(
            probe.polls("A") > 0,
            "old scheduler must keep running after failed rebuild"
        );
        assert!(running.polls("A") >= before);

        running.shutdown().await;
    }

    /// 组身份比对：等价集合不触发重建，任何一维度变化都要触发。
    #[test]
    fn groups_changed_detects_group_identity_differences() {
        let base = vec![group("A", 100, &["a1", "a2"]), group("B", 1000, &["b1"])];

        assert!(
            !groups_changed(&base, &base.clone()),
            "same list is a no-op"
        );
        // 顺序不同但内容一致 → 等价（比对前按组名排序）。
        let shuffled = vec![group("B", 1000, &["b1"]), group("A", 100, &["a1", "a2"])];
        assert!(!groups_changed(&base, &shuffled), "order must not matter");

        // 组名 / 点位 / 周期 / 超时 任一变化 → 有差异。
        assert!(groups_changed(
            &base,
            &[group("C", 100, &["a1", "a2"]), group("B", 1000, &["b1"])]
        ));
        assert!(groups_changed(
            &base,
            &[group("A", 100, &["a1"]), group("B", 1000, &["b1"])]
        ));
        assert!(groups_changed(
            &base,
            &[group("A", 250, &["a1", "a2"]), group("B", 1000, &["b1"])]
        ));
        assert!(groups_changed(
            &base,
            &[
                group("A", 100, &["a1", "a2"])
                    .with_timeout(Duration::from_millis(50))
                    .expect("valid timeout"),
                group("B", 1000, &["b1"])
            ]
        ));
        // 数量变化（加组 / 减组）→ 有差异。
        assert!(groups_changed(&base, &[group("A", 100, &["a1", "a2"])]));
        assert!(groups_changed(
            &base,
            &[group("A", 100, &["a1", "a2"]), group("C", 100, &["c1"])]
        ));
    }
}

/// 网关**计划重启**调度（配置 `[ops] scheduled_restart_at = "HH:MM"`，每日定时；
/// 空 / 未配置 = 不启用）。
///
/// ## 语义
/// - `at` 为**本机本地时区**的每日时刻（`HH:MM`）；
/// - 每 [`TICK_SECS`] 检查一次，命中即走与 `POST /api/ops/restart` 完全相同的
///   优雅停机通道（`DaemonShared::request_shutdown`），由 Supervisor / 服务管理器
///   负责拉起；重启后配置重新装载，调度自然生效；
/// - **同日只触发一次**：命中后把日期写进 `[ops] last_restart_date`，当日后续
///   检查一律跳过；跨日（日期键变化）自动恢复。
///
/// ## 安全口径（fail-closed）
/// 配置路径未装配（无法落盘日期标记）或标记落盘失败时**绝不触发**重启——否则
/// 标记无法持久化，会导致每 tick 反复重启。
pub mod scheduled_restart {
    use std::sync::Arc;

    use tokio::task::JoinHandle;
    use tokio::time::{interval, Duration};
    use tracing::{info, warn};

    use crate::bootstrap::DaemonShared;

    /// 检查周期（秒）。
    const TICK_SECS: u64 = 30;

    /// 本机本地墙钟（格式化后的本地时间；`HH:MM` 命中判定与日期键的依据）。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct LocalClock {
        pub year: u16,
        pub month: u8,
        pub day: u8,
        pub hour: u8,
        pub minute: u8,
    }

    /// 生成本地墙钟（不引第三方时间库：Windows 走 `kernel32!GetLocalTime`，
    /// 其余平台走 POSIX `localtime_r`）。
    pub fn local_clock() -> LocalClock {
        #[cfg(windows)]
        {
            #[repr(C)]
            #[derive(Default, Clone, Copy)]
            struct WinSystemTime {
                w_year: u16,
                w_month: u16,
                w_day_of_week: u16,
                w_day: u16,
                w_hour: u16,
                w_minute: u16,
                _w_second: u16,
                _w_milliseconds: u16,
            }
            extern "system" {
                fn GetLocalTime(lp_system_time: *mut WinSystemTime);
            }
            let mut st = WinSystemTime::default();
            unsafe { GetLocalTime(&mut st) };
            LocalClock {
                year: st.w_year,
                month: (st.w_month as u8).clamp(1, 12),
                day: (st.w_day as u8).clamp(1, 31),
                hour: (st.w_hour as u8).clamp(0, 23),
                minute: (st.w_minute as u8).clamp(0, 59),
            }
        }
        #[cfg(not(windows))]
        {
            use std::ffi::c_int;

            /// POSIX `struct tm` 的前 9 个成员（`localtime_r` 只写这些，够拼日期键）。
            #[repr(C)]
            #[derive(Default, Clone, Copy)]
            struct PosixTm {
                tm_sec: c_int,
                tm_min: c_int,
                tm_hour: c_int,
                tm_mday: c_int,
                tm_mon: c_int,
                tm_year: c_int,
                tm_wday: c_int,
                tm_yday: c_int,
                tm_isdst: c_int,
            }
            extern "C" {
                fn time(timep: *mut i64) -> i64;
                fn localtime_r(timep: *const i64, result: *mut PosixTm) -> *mut PosixTm;
            }
            let mut raw: i64 = 0;
            let secs = unsafe { time(&mut raw) };
            let secs = if secs == -1 { 0 } else { secs };
            let mut tm = PosixTm::default();
            let ptr = unsafe { localtime_r(&secs, &mut tm) };
            let field = |get: fn(&PosixTm) -> c_int, fallback: u8| -> u8 {
                if ptr.is_null() {
                    fallback
                } else {
                    // 只读合法值域，越界回退兜底（绝不 panic）。
                    u8::try_from(get(unsafe { &*ptr })).unwrap_or(fallback)
                }
            };
            LocalClock {
                year: 1900 + u16::try_from(field(|t| t.tm_year, 0)).unwrap_or(0),
                month: field(|t| t.tm_mon + 1, 1),
                day: field(|t| t.tm_mday, 1),
                hour: field(|t| t.tm_hour, 0),
                minute: field(|t| t.tm_min, 0),
            }
        }
    }

    /// 解析每日时刻 `"HH:MM"`（允许前后空格与 `H:MM` 简写）→ `(hour, minute)`；
    /// 非法 → `None`。
    pub fn parse_at(raw: &str) -> Option<(u8, u8)> {
        let s = raw.trim();
        let (h, m) = s.split_once(':')?;
        let hour = h.parse::<u8>().ok()?;
        let minute = m.parse::<u8>().ok()?;
        if hour > 23 || minute > 59 {
            return None;
        }
        Some((hour, minute))
    }

    /// 本地日期键 `"YYYY-MM-DD"`（同日判定的依据）。
    pub fn date_key(now: &LocalClock) -> String {
        format!("{:04}-{:02}-{:02}", now.year, now.month, now.day)
    }

    /// 是否**应当**触发：`at` 合法 + 当前本地时刻正好命中 + 当日尚未触发过。
    ///
    /// 纯函数（时间由调用方注入），便于单测覆盖「当日已触发不重复」口径。
    pub fn should_fire(at: &str, last_restart_date: &str, now: &LocalClock) -> bool {
        let Some((hour, minute)) = parse_at(at) else {
            return false;
        };
        if now.hour != hour || now.minute != minute {
            return false;
        }
        // 日期键相同 = 同一天，已触发过 → 不再重复。
        !last_restart_date
            .trim()
            .eq_ignore_ascii_case(&date_key(now))
    }

    /// 启动计划重启后台任务（返回 `JoinHandle`，循环不退出直到进程停机）。
    pub fn spawn(daemon: DaemonShared) -> JoinHandle<()> {
        tokio::spawn(run(Arc::new(daemon)))
    }

    async fn run(daemon: Arc<DaemonShared>) {
        let mut ticker = interval(Duration::from_secs(TICK_SECS));
        // 首拍立即等一个周期（`interval` 首拍是瞬时），避免装配期多余检查。
        loop {
            ticker.tick().await;
            once(&daemon);
        }
    }

    /// 单趟检查（同步执行：落盘 + 停机请求都是非 `await` 阻塞操作）。
    ///
    /// `true` = 本趟已请求重启。
    fn once(daemon: &Arc<DaemonShared>) -> bool {
        let snapshot = daemon.config_snapshot();
        let at = snapshot.ops.scheduled_restart_at.clone();
        if at.trim().is_empty() {
            return false;
        }
        let Some(path) = daemon.config_path() else {
            warn!(
                at = %at.trim(),
                "scheduled restart: config path not bound; refusing to fire (fail-closed)"
            );
            return false;
        };
        let now = local_clock();
        if !should_fire(&at, &snapshot.ops.last_restart_date, &now) {
            return false;
        }
        // 先落盘日期标记：落盘失败 ⇒ 不重启（避免每 tick 反复重启）。
        let mut next = (*snapshot).clone();
        next.ops.last_restart_date = date_key(&now);
        if let Err(err) = next.save(&path) {
            warn!(
                error = %err,
                at = %at.trim(),
                "scheduled restart: date marker not persisted; restart withheld"
            );
            return false;
        }
        let version = daemon.config_shared().replace(next);
        audit_scheduled_restart(daemon, "graceful shutdown requested by scheduled restart");
        info!(
            at = %at.trim(),
            date = %date_key(&now),
            config_version = version,
            "scheduled restart: graceful shutdown requested"
        );
        daemon.request_shutdown();
        true
    }

    /// 计划重启的持久审计落点（actor 固定为调度器自身，可与管理面重启区分）。
    fn audit_scheduled_restart(daemon: &Arc<DaemonShared>, detail: &str) {
        if let Some(logger) = daemon.audit_logger() {
            if let Err(err) = logger.record(
                "scheduled_restart",
                crate::audit::AuditEventType::ConfigChange,
                "accepted",
                detail,
            ) {
                warn!(error = %err, "scheduled restart: persistent audit record failed");
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// 构造一个本地时刻（测试夹具）。
        fn clock(year: u16, month: u8, day: u8, hour: u8, minute: u8) -> LocalClock {
            LocalClock {
                year,
                month,
                day,
                hour,
                minute,
            }
        }

        #[test]
        fn parse_at_accepts_hh_mm_and_rejects_garbage() {
            assert_eq!(parse_at("02:30"), Some((2, 30)));
            assert_eq!(parse_at("  23:59  "), Some((23, 59)));
            assert_eq!(parse_at("7:5"), Some((7, 5)));
            assert_eq!(parse_at("24:00"), None, "hour must be < 24");
            assert_eq!(parse_at("02:60"), None, "minute must be < 60");
            assert_eq!(parse_at(""), None, "empty = disabled");
            assert_eq!(parse_at("0200"), None, "must be HH:MM");
            assert_eq!(parse_at("2-30"), None);
        }

        #[test]
        fn fires_only_on_exact_time_and_once_per_day() {
            // 配置 02:30；当天 02:31 → 不触发（只按整刻命中，不做区间判定）。
            assert!(!should_fire("02:30", "", &clock(2026, 9, 27, 2, 31)));
            // 02:30 命中且当日未触发 → 触发。
            assert!(should_fire("02:30", "", &clock(2026, 9, 27, 2, 30)));
            // 当日已触发（日期键相同）→ 不重复。
            assert!(!should_fire(
                "02:30",
                "2026-09-27",
                &clock(2026, 9, 27, 2, 30)
            ));
            // 跨日 → 恢复（新的一天）。
            assert!(should_fire(
                "02:30",
                "2026-09-27",
                &clock(2026, 9, 28, 2, 30)
            ));
            // 非法时刻永不触发。
            assert!(!should_fire("nope", "", &clock(2026, 9, 27, 2, 30)));
        }

        #[test]
        fn date_key_zero_pads_local_date() {
            assert_eq!(date_key(&clock(2026, 9, 27, 2, 30)), "2026-09-27");
            assert_eq!(date_key(&clock(2026, 1, 5, 2, 30)), "2026-01-05");
        }
    }
}

/// 网关**系统更新（OTA）轮询**（配置 `[gateway.ota]`；与授权端
/// `GET /updates/manifest` 对接）。
///
/// ## 语义
/// - `[gateway.ota].enabled = false`（缺省）/ **未就绪**（[`OtaSection::is_ready`]：
///   无生效端点——`manifest_url` 省略且 `cloud_url` 也空——或缺 `signing_key_b64`）
///   → **no-op**：不发任何网络请求、不报错；启动时记一条 `info!`（关闭）或
///   `warn!`（已启用但未就绪，附配置键名与 env 通道）。
/// - 生效端点由 [`OtaSection::effective_manifest_url`] 解析：显式 `manifest_url` 优先，
///   否则按 `{cloud_url}/updates/manifest` 推导（容器形态无需写 `manifest_url`）。
/// - 启用 → **首个周期到来时才执行**：先 `sleep(poll_interval_secs)` 再检查
///   （避免启动瞬间打请求 / 启动风暴）；周期每轮从配置快照重读，热重载即生效。
/// - 每轮：GET `manifest_url` → 读 `available` → 复用**全进程唯一**
///   [`crate::ota::OtaManager`]（由 bootstrap 用落盘 `FileOtaStore` 装配）的
///   解析 + 版本单调性 + Ed25519 验签管线 → 写入落盘 pending 槽；
///   `available = false`（授权端诚实空态）**不计失败**，不重试风暴；
///   网络失败 / 验签失败 / 报文非法 / 版本不新 → `warn!` 带**真实原因**，旧版本不受影响。
/// - 公钥热重载：每轮比对 `signing_key_b64`，变化时就地替换管理器的验签公钥
///   （版本真相始终来自落盘 `meta.json`，无需重建管理器）。
///
/// ## V1 限制（诚实声明）
/// - 下载通道仅支持 `http://` 明文（无 TLS；完整性由 Ed25519 验签兜底，
///   机密性无保障，详见 [`crate::ota`] 模块文档）；
/// - pending / current 槽已**落盘持久**（`<data_dir>/ota/`），进程重启不丢；
///   启动确认（commit / 回滚）由 `bootstrap.rs` 按「上次运行是否健康」的落盘标记
///   驱动（见该模块 OTA 启动健康标记）。
pub mod ota_poll {
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::task::JoinHandle;
    use tracing::{info, warn};

    use crate::bootstrap::DaemonShared;
    use crate::config::OtaSection;
    use crate::ota::{verifying_key_from_b64, OtaManager, OtaPollOutcome};

    /// 启动 OTA 轮询后台任务（返回 `JoinHandle`，循环不退出直到进程停机）。
    pub fn spawn(daemon: DaemonShared) -> JoinHandle<()> {
        tokio::spawn(run(Arc::new(daemon)))
    }

    async fn run(daemon: Arc<DaemonShared>) {
        // 启动提示（一次）：关闭 / 已启用但未就绪都明确说明，绝不静默。
        {
            let snapshot = daemon.config_snapshot();
            log_config_state(
                &snapshot.gateway.ota,
                snapshot.gateway.licensing.cloud_url.as_deref(),
            );
        }

        // 已绑定的签名公钥：配置热重载更换公钥时就地替换（版本真相始终来自落盘 meta，
        // 无需重建管理器）。
        let mut bound_key: Option<String> = None;
        loop {
            // 首个周期到来才执行（先 sleep 再检查，避免启动风暴）。
            let interval = daemon
                .config_snapshot()
                .gateway
                .ota
                .poll_interval_secs
                .max(1);
            tokio::time::sleep(Duration::from_secs(interval)).await;

            // 睡醒后重读配置（热重载即时生效）。
            let snapshot = daemon.config_snapshot();
            let section = snapshot.gateway.ota.clone();
            let cloud_url = snapshot.gateway.licensing.cloud_url.clone();
            let cloud = cloud_url.as_deref();
            if !section.is_ready(cloud) {
                bound_key = None;
                continue;
            }
            // 生效端点：显式 manifest_url 优先，否则按 cloud_url 推导（不写回配置）。
            let Some(url) = section.effective_manifest_url(cloud) else {
                bound_key = None;
                continue;
            };
            let key_b64 = section
                .signing_key_b64
                .as_deref()
                .unwrap_or("")
                .trim()
                .to_string();

            // 全进程唯一实例（由 bootstrap 用落盘 FileOtaStore 装配）；未接线 → 跳过。
            let Some(manager) = daemon.ota_manager() else {
                warn!("ota: shared OtaManager not wired; upgrade check skipped");
                continue;
            };

            if bound_key.as_deref() != Some(key_b64.as_str()) {
                match verifying_key_from_b64(&key_b64) {
                    Ok(key) => {
                        manager.lock().await.set_verifying_key(key);
                        bound_key = Some(key_b64.clone());
                        info!(url = %url, "ota: poller bound to manifest endpoint");
                    }
                    Err(err) => {
                        warn!(
                            error = %err,
                            "ota: signing_key_b64 is not a usable Ed25519 public key; \
                             upgrade check skipped"
                        );
                        bound_key = None;
                        continue;
                    }
                }
            }

            // 单轮检查（未就绪 → `None`；此处已 `is_ready`，故必为 `Some`）。
            let mut guard = manager.lock().await;
            let Some(outcome) = check_once(&section, cloud, &mut guard).await else {
                continue;
            };
            match outcome {
                OtaPollOutcome::Applied { version } => info!(
                    version,
                    "ota: new version staged to pending slot (restart required to apply)"
                ),
                OtaPollOutcome::NoUpdate { reason } => info!(
                    reason = %reason,
                    "ota: no update available (authoritative empty state)"
                ),
                OtaPollOutcome::AlreadyPending { version } => info!(
                    version = %version,
                    "ota: a pending version is already staged; skipping"
                ),
                OtaPollOutcome::Rejected { reason } => warn!(
                    url = %url,
                    reason = %reason,
                    "ota: upgrade check failed (running version unaffected)"
                ),
            }
        }
    }

    /// 单轮检查（未就绪 → `None`，**不产生任何网络动作**）。
    async fn check_once(
        section: &OtaSection,
        cloud_url: Option<&str>,
        manager: &mut OtaManager,
    ) -> Option<OtaPollOutcome> {
        if !section.is_ready(cloud_url) {
            return None;
        }
        let url = section.effective_manifest_url(cloud_url)?;
        Some(manager.poll_and_stage(&url).await)
    }

    /// 启动时的一次性配置状态提示（关闭 / 未就绪 / 已就绪）。
    fn log_config_state(section: &OtaSection, cloud_url: Option<&str>) {
        if section.is_ready(cloud_url) {
            info!(
                url = %section.effective_manifest_url(cloud_url).unwrap_or_default(),
                interval_secs = section.poll_interval_secs.max(1),
                current_version = section.current_version,
                "ota: upgrade checks enabled"
            );
        } else if section.enabled {
            // 已启用但缺端点 / 公钥 → 明确 warn（含配置键名与 env 通道），绝不静默。
            warn!("ota: {}", section.config_hint());
        } else {
            info!("ota: disabled ([gateway.ota].enabled = false); no upgrade checks");
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::net::SocketAddr;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use base64::Engine as _;
        use ed25519_dalek::SigningKey;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        use crate::ota::{InMemoryOtaStore, OtaStore};

        /// 计数型 mock HTTP 服务器：每收到一次请求即 +1，并回放固定响应。
        async fn counting_mock(counter: Arc<AtomicUsize>) -> SocketAddr {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let addr = listener.local_addr().expect("local addr");
            tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        return;
                    };
                    let counter = Arc::clone(&counter);
                    tokio::spawn(async move {
                        let mut buf = Vec::new();
                        let mut chunk = [0u8; 1024];
                        loop {
                            match stream.read(&mut chunk).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => {
                                    buf.extend_from_slice(&chunk[..n]);
                                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                        break;
                                    }
                                }
                            }
                        }
                        counter.fetch_add(1, Ordering::SeqCst);
                        let body = br#"{"available":false,"reason":"none"}"#;
                        let resp =
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
                        let mut out = resp.into_bytes();
                        out.extend_from_slice(body);
                        let _ = stream.write_all(&out).await;
                    });
                }
            });
            addr
        }

        fn manager() -> OtaManager {
            let store = Arc::new(InMemoryOtaStore::default());
            OtaManager::new(
                store as Arc<dyn OtaStore>,
                SigningKey::from_bytes(&[0x7au8; 32]).verifying_key(),
                1,
            )
        }

        /// `enabled = false` → 单轮检查 no-op，**绝不发请求**、不报错。
        #[tokio::test]
        async fn disabled_section_performs_no_download() {
            let counter = Arc::new(AtomicUsize::new(0));
            let addr = counting_mock(Arc::clone(&counter)).await;
            let section = OtaSection {
                enabled: false,
                manifest_url: Some(format!("http://{addr}/updates/manifest")),
                signing_key_b64: Some("AAAA".to_string()),
                poll_interval_secs: 1,
                current_version: 1,
            };
            let mut mgr = manager();
            let outcome = check_once(&section, None, &mut mgr).await;
            assert!(outcome.is_none(), "未启用 = no-op");
            assert_eq!(
                counter.load(Ordering::SeqCst),
                0,
                "disabled 时不得发出任何网络请求"
            );
        }

        /// `enabled = true` 但缺端点（无 manifest_url 且无 cloud_url）/ 缺公钥
        /// → 判定「未就绪」→ no-op（诚实降级，不 panic）。
        #[tokio::test]
        async fn enabled_but_unconfigured_is_noop() {
            let counter = Arc::new(AtomicUsize::new(0));
            let addr = counting_mock(Arc::clone(&counter)).await;
            let mut mgr = manager();

            let no_url = OtaSection {
                enabled: true,
                manifest_url: None,
                signing_key_b64: Some("AAAA".to_string()),
                ..OtaSection::default()
            };
            assert!(check_once(&no_url, None, &mut mgr).await.is_none());

            let no_key = OtaSection {
                enabled: true,
                manifest_url: Some(format!("http://{addr}/updates/manifest")),
                signing_key_b64: None,
                ..OtaSection::default()
            };
            assert!(check_once(&no_key, Some("http://license.test/x"), &mut mgr)
                .await
                .is_none());
            assert_eq!(
                counter.load(Ordering::SeqCst),
                0,
                "未就绪时不得发出任何网络请求"
            );
        }

        /// `manifest_url` 省略 + `cloud_url` 有值 → poller 按推导端点发起检查
        /// （不再误判「未配置」）；推导成功即真实发出一次请求。
        #[tokio::test]
        async fn poller_derives_manifest_url_from_cloud_url() {
            let counter = Arc::new(AtomicUsize::new(0));
            let addr = counting_mock(Arc::clone(&counter)).await;
            let section = OtaSection {
                enabled: true,
                manifest_url: None,
                signing_key_b64: Some("AAAA".to_string()),
                ..OtaSection::default()
            };
            let mut mgr = manager();
            let cloud = format!("http://{addr}/licensing/");
            let outcome = check_once(&section, Some(&cloud), &mut mgr).await;
            assert!(
                matches!(outcome, Some(OtaPollOutcome::NoUpdate { .. })),
                "cloud_url 推导成功后应发起检查并得到诚实空态: {outcome:?}"
            );
            assert_eq!(
                counter.load(Ordering::SeqCst),
                1,
                "应恰好发出一次请求（推导端点被真实使用）"
            );
        }

        /// 轮询绑定公钥：坏公钥 → ConfigError（不 panic）；合法公钥 → 可解析。
        #[test]
        fn poller_rejects_bad_signing_key() {
            assert!(verifying_key_from_b64("AAAA").is_err(), "坏公钥必须被拒");
            let key = SigningKey::from_bytes(&[0x11u8; 32]).verifying_key();
            let b64 = base64::engine::general_purpose::STANDARD.encode(key.to_bytes());
            assert!(verifying_key_from_b64(&b64).is_ok(), "合法公钥可解析");
        }
    }
}
