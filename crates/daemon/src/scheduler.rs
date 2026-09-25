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
/// （[`GroupScheduler::spawn_group`]）共用此函数，保证两条路径行为与统计口径一致。
async fn execute_poll<H: PollHandler>(
    handler: &H,
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
                Self::spawn_group(handler.clone(), config, group_stats, None, None)
            })
            .collect();
        RunningScheduler {
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
                Self::spawn_group(
                    handler.clone(),
                    config,
                    group_stats,
                    Some(governor.clone()),
                    Some(rows_provider.clone()),
                )
            })
            .collect();
        RunningScheduler {
            names,
            handles,
            stats,
        }
    }

    /// 单组任务主循环：等拍 → 轮询 → 记账 → （可选）观测水位节流 → 继续（失败不退出）。
    fn spawn_group(
        handler: Arc<H>,
        config: GroupConfig,
        stats: Arc<GroupStats>,
        governor: Option<Arc<Mutex<AcquisitionGovernor>>>,
        rows_provider: Option<Arc<dyn Fn() -> usize + Send + Sync>>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            // 首拍推迟一个周期（设计决策 2）+ 阻塞后不补采（设计决策 3）。
            let mut ticker = interval_at(Instant::now() + config.interval, config.interval);
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
}

/// 已启动的调度器句柄：持有各组任务句柄与共享统计表。
#[derive(Debug)]
pub struct RunningScheduler {
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
}
