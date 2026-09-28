//! 数据面接线（task-61 验收 D-14 修复）：南向采集样本 → 管线变换 → 北向投递。
//!
//! # 背景
//! 669896e 后南向（[`crate::southbound::DevicePollHandler`]）与北向
//! （[`crate::north::runtime::NorthRuntime`]）在生产形态各自装配，但全仓没有
//! 生产调用方把采集到的样本投递进 [`NorthRuntime::submit`]——北向 MQTT 客户端
//! 真实连上 broker 却 0 PUBLISH。本模块补上这最后一公里。
//!
//! # 数据流（每一跳）
//! 1. 调度器每拍调 [`PollHandler::poll`] → 本模块委托内层 [`DevicePollHandler`]
//!    做南向驱动批量读，取回**样本**（解码后数值 + 质量码 + 时间戳）；
//! 2. [`AcquisitionPipeline::ingest_physical`]：质量码归一 + 单位换算 + 死区过滤
//!    （普通点位直传；公式点输入按最新工程值记账——V1 配置无公式段，公式引擎
//!    恒 `None`，接线面已留好）；
//! 3. [`AcquisitionPipeline::finish_cycle`]：周期屏障统一求值派生点（有则并入批次）；
//! 4. `sample_to_data_point` 归一为 `DataPoint` → 组装 [`TelemetryBatch`]；
//! 5. **按出口编码**（`[[outlets]].encoding`，protobuf 默认 / json 可选）：每路
//!    出口用各自的 [`BatchEncoder`] 独立编码同一批次（两套编码语义一致由
//!    `encoder.rs` 既有 golden / 双编码测试冻结）；
//! 6. [`NorthRuntime::submit`]：进该出口的有界发送队列——背压（超水位落盘
//!    降级）、补发、审计全部复用既有机制，本模块**不新造**。
//!
//! # 为什么编码在数据面做（单一职责判定依据）
//! [`NorthRuntime::submit`] 的入参是**最终字节**；发送队列与补发路径发布的
//! payload 原样上线（`pump_send` / `pump_replay` 不做任何编码），出口的
//! `encoding` 声明只被单点直发路径 `publish_sample` 消费（生产驱动不经过）。
//! 因此编码**必须**发生在 `submit` 之前，即数据面；按出口配置选择编码器即
//! 保持「每路出口独立可选」的既有语义。
//!
//! # 红线遵守
//! - **授权判定语义零改动**：本模块只做「投递」，发送与否由出口驱动任务每拍问
//!   [`crate::north::runtime::NorthForwardGate`]（在 submit **下游**）——不实现
//!   第二个配额判定；Degraded 期间发送队列照常受理（超限自然落盘降级，水位有界）。
//! - **不碰已持久化批次**：本模块只 `submit` 新鲜样本，绝不把 OfflineQueue 中的
//!   批次再推入发送队列（补发只走既有 replay 路径）。
//! - **采集路径永不阻塞**：`forward` 全程同步、O(已发射点数)，无全表扫描 / 无界
//!   循环；发送队列 push 只做水位比较 + 一次有界落盘调用。
//! - **绝不 panic**：管线配置非法 → 降级为「只采不转」（error! 可观测）；锁中毒
//!   取回内部数据；submit 拒绝 / 编码失败只计数 + warn。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard, OnceLock, PoisonError};

use async_trait::async_trait;
use base64::Engine as _;
use protocol_proto::TelemetryBatch;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use crate::alarm::{AlarmEngine, AlarmRecord, AlarmStore};
use crate::backpressure::{AuditLog, PushOutcome};
use crate::config::GatewayConfig;
use crate::config::{ConfigShared, RuleConfig};
use crate::error::DaemonResult;
use crate::mgmt::health::{now_ms, DeviceHealthRegistry};
use crate::north::encoder::{encoder_for, sample_to_data_point, BatchEncoder, JsonEncoder};
use crate::north::runtime::NorthRuntime;
use crate::offline_queue::{Clock, SystemClock};
use crate::pipeline::{
    AcquisitionPipeline, DataProcessor, PointConfig as PipelinePointConfig, ProcessedSample,
    RawSample,
};
use crate::rules::{RoutedMessage, RuleEngine};
use crate::scheduler::PollHandler;

/// 取锁并在中毒时取回内部数据（**绝不 panic**；与 `north/runtime.rs` 同口径）。
fn lock_or_recover<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => PoisonError::into_inner(poisoned),
    }
}

/// 单个北向出口的数据面泳道：出口名 + 该路编码器 + 批次序号分配器。
struct OutletLane {
    /// 出口名（`[[outlets]].name`，`NorthRuntime::submit` 的注册表键）。
    name: String,
    /// 该路出口的载荷编码器（protobuf 默认 / json 可选）。
    encoder: Box<dyn BatchEncoder>,
    /// 发送队列批次序号分配器（本泳道内单调递增，从 1 起）。
    seq: AtomicU64,
}

/// 数据面运行统计（可观测；原子计数快照）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DataPlaneStats {
    /// 执行过 forward 的采集拍数（样本非空才计）。
    pub forward_cycles: u64,
    /// 北向运行期就绪前产生的拍数（启动竞态窗口；就绪后恒 0 增长）。
    pub pre_runtime_cycles: u64,
    /// 物理点经死区后发射的点数。
    pub physical_emitted: u64,
    /// 派生点（公式）发射的点数。
    pub derived_emitted: u64,
    /// 进入发送队列（内存）的批次数。
    pub admitted: u64,
    /// 落盘降级的批次数（发送队列超水位 → queue.db）。
    pub spilled: u64,
    /// 硬上限拒绝（数据交还调用方，已审计）的批次数。
    pub rejected: u64,
    /// 批次编码失败次数（出口侧问题不影响其余出口）。
    pub encode_errors: u64,
    /// 转发规则求值次数（已启用规则非空时每拍每个发射样本 +1）。
    pub rules_evaluated: u64,
    /// 转发规则命中的消息条数（`DO` 动作产出的 `RoutedMessage` 总数）。
    pub rules_routed: u64,
    /// 喂给告警引擎的样本数（已启用告警规则非空时每拍每个发射样本 +1）。
    pub alarms_evaluated: u64,
    /// 告警引擎产出的记录数（新触发 / 续期 / 恢复）。
    pub alarms_fired: u64,
}

/// 数据面桥接（`PollHandler` 装饰器）：南向采集 → 管线变换 → 北向投递。
///
/// 由 bootstrap 在装配期构造（包裹 [`crate::southbound::DevicePollHandler`]）并
/// 注入调度器；北向运行期就绪后调 [`Self::attach`] 挂载句柄（晚绑定：调度器先于
/// 北向启动，就绪前样本照常采集、只不做北向投递）。
pub struct NorthDataPlane {
    /// 内层采集动作（真实南向读；本类型只做转发不重采集）。
    inner: Arc<dyn PollHandler>,
    /// 采集→分发编排器（换算 + 死区 + 公式周期屏障；`None` = 点位配置非法，
    /// 降级为「只采不转」，error! 已在构造期记录）。
    pipeline: Option<StdMutex<AcquisitionPipeline>>,
    /// 出口泳道（保序；attach 时剔除北向启动期被拒绝的出口）。
    lanes: StdMutex<Vec<OutletLane>>,
    /// 北向运行期句柄（晚绑定；`None` = 尚未 attach）。
    runtime: OnceLock<Arc<NorthRuntime>>,
    /// 网关标识（批次 `gateway_id` 字段）。
    gateway_id: String,
    /// 时钟（采集时间戳；管线审计时间戳同源）。
    clock: Arc<dyn Clock>,
    /// 原子计数器。
    counters: DataPlaneCounters,
    /// 实时遥测广播（task 52）：解码后逐点遥测扇出给管理面 `/api/stream` 订阅者。
    ///
    /// 仅 `Err`/`None` 丢弃、不 panic（broadcast 无订阅者 / 编码异常均为正常路径）。
    live_tx: broadcast::Sender<LiveTelemetry>,
    /// 设备真实运行健康度注册表（需求 1）：每拍成功 / 失败事实在此落账。
    health: Arc<DeviceHealthRegistry>,
    /// 不推送点位集合（需求 6）：`device_id -> {point_id}`，`push_enabled=false` 的
    /// 点位**照常采集 / 进实时流**，但被排除在北向转发批次之外。
    ///
    /// 键用 device_id 分桶 + 内层 point_id，避免每拍为查表而拼接字符串（热路径零分配）。
    no_push: HashMap<String, HashSet<String>>,
    /// 配置共享句柄（**规则引擎热重建的真相源**：管理面改规则后
    /// `ConfigShared::replace` 推新快照 + 新版本号，下一拍据此重建引擎）。
    config: Arc<ConfigShared>,
    /// 转发规则引擎（`[[rules]]` 已启用规则的求值器；`None` = 无启用的规则或
    /// 规则集非法降级）。构造期即完成全部语义校验（JSONPath / topic / DAG）；
    /// 用 `StdMutex` 而非 `OnceLock`：配置版本变化时要能**原地重建**（启停 /
    /// 增删规则的热生效），锁中毒按既有口径取回内部数据（零 panic）。
    rules: StdMutex<Option<RuleEngine>>,
    /// 规则引擎当前对应的配置版本号（版本未变则复用既有引擎，热路径零重建）。
    rule_version: AtomicU64,
    /// 告警引擎（`[alarms]` 已启用规则；空装配 = 无告警数据源，本阶段整体跳过）。
    ///
    /// 与 `rules` 同构：配置版本变化即原地重建；告警记录另存 [`Self::alarms`]
    /// 仓库，不随引擎重建丢（否则每次改规则，页面上正在看的告警就凭空消失）。
    alarm_engine: StdMutex<AlarmEngine>,
    /// 告警引擎当前对应的配置版本号。
    alarm_version: AtomicU64,
    /// 告警记录仓库（数据面写 / 管理面 `GET /api/alerts` 读；进程内 historian）。
    alarms: Arc<AlarmStore>,
}

/// 由配置里的 `[[rules]]` 构造规则引擎：只吃 **已启用** 规则，按 `priority`
/// 升序（数字越小越先匹配）求值。
///
/// 规则集语义非法（未知字段路径 / 空 publish topic / `depends_on` 成环或指向
/// 未知 id / 重复 id）→ `warn` 后**降级为「无规则」**，绝不影响采集与北投递：
/// 半写的规则会把全部样本转发出去，那比「没有规则」危险得多。
fn build_rule_engine(config: &GatewayConfig) -> Option<RuleEngine> {
    let mut enabled: Vec<&RuleConfig> = config.rules.iter().filter(|rule| rule.enabled).collect();
    if enabled.is_empty() {
        return None;
    }
    enabled.sort_by_key(|rule| rule.priority);
    match RuleEngine::from_rules(enabled.iter().map(|rule| rule.to_rule()).collect()) {
        Ok(engine) => Some(engine),
        Err(err) => {
            warn!(error = %err, "dataplane: rule engine rejected the rule set; rules disabled this process");
            None
        }
    }
}

/// 由配置里的 `[alarms]` 构造告警引擎（只吃 **enabled** 规则；语义非法的规则
/// 由引擎装配期跳过 + warn，绝不静默塞进求值路径）。
///
/// `[alarms]` 缺省 / `enabled = false` / 无可用规则 → 空引擎 ⇒ 数据面告警阶段
/// 整拍跳过（热路径零开销，与无告警配置的网关行为完全一致）。
fn build_alarm_engine(config: &GatewayConfig) -> AlarmEngine {
    let (engine, skipped) = AlarmEngine::from_config(config);
    if !skipped.is_empty() {
        warn!(
            skipped = skipped.len(),
            "dataplane: alarm engine skipped unusable alarm rule(s); the remaining rules still evaluate"
        );
    }
    engine
}

/// 原子计数器组（内部可变、快照只读）。
#[derive(Debug, Default)]
struct DataPlaneCounters {
    forward_cycles: AtomicU64,
    pre_runtime_cycles: AtomicU64,
    physical_emitted: AtomicU64,
    derived_emitted: AtomicU64,
    admitted: AtomicU64,
    spilled: AtomicU64,
    rejected: AtomicU64,
    encode_errors: AtomicU64,
    rules_evaluated: AtomicU64,
    rules_routed: AtomicU64,
    /// 本拍喂给告警引擎的样本数（无告警配置时不累加）。
    alarms_evaluated: AtomicU64,
    /// 本拍告警引擎产出（新触发 / 续期 / 恢复）的记录数。
    alarms_fired: AtomicU64,
}

impl DataPlaneCounters {
    fn bump(slot: &AtomicU64) {
        slot.fetch_add(1, Ordering::Relaxed);
    }

    fn add(slot: &AtomicU64, delta: u64) {
        slot.fetch_add(delta, Ordering::Relaxed);
    }

    fn get(slot: &AtomicU64) -> u64 {
        slot.load(Ordering::Relaxed)
    }
}

impl NorthDataPlane {
    /// 从配置构建数据面桥接（包裹 `inner` 采集动作；纯内存、不失败）。
    ///
    /// - 点位表 → 管线直通配置（schema 无单位 / 换算列 → 不换算、不过滤；
    ///   `source_id` = `point_id`，南向点位标识即管线源标识）；
    /// - 点位配置非法（如 `point_id` 重复）→ 管线降级为 `None`（error! 可解释），
    ///   采集与调度完全不受影响，只是不做北向变换；
    /// - 出口泳道按 `[[outlets]]` 全量构建（编码按该路 `encoding`；北向启动期
    ///   被拒绝的出口在 [`Self::attach`] 时剔除）。
    ///
    /// `live_tx` 为管理面实时遥测广播发送端（`DaemonShared::live_sender` 注入）；
    /// `health` 为设备健康度注册表（`DaemonShared::health_registry` 注入），
    /// 采集成功 / 失败逐拍落账（需求 1）。
    #[must_use]
    pub fn new(
        inner: Arc<dyn PollHandler>,
        config: &GatewayConfig,
        config_shared: Arc<ConfigShared>,
        live_tx: broadcast::Sender<LiveTelemetry>,
        health: Arc<DeviceHealthRegistry>,
        alarms: Arc<AlarmStore>,
    ) -> Self {
        let gateway_id = config.gateway.gateway_id.clone();
        let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
        // 转发规则引擎：`[[rules]]` 在这条链路上第一次成为生产路径（此前
        // `RuleEngine` 无任何生产调用方——配置里的规则只是死的 TOML 行）。
        // 纪录的引擎版本号 = 当前配置版本，避免首拍无谓重建。
        let rule_version = config_shared.version();
        // 告警引擎：同规则引擎一处装配，版本号同样取当前配置版本（首拍不重建）。
        let alarm_version = config_shared.version();

        // 需求 6：收集 `push_enabled = false` 的点位（device_id -> {point_id}）。
        // 空集合（全部推送）时不做任何过滤，热路径零额外开销。
        let mut no_push: HashMap<String, HashSet<String>> = HashMap::new();
        for point in &config.points {
            if !point.push_enabled {
                no_push
                    .entry(point.device_id.clone())
                    .or_default()
                    .insert(point.point_id.clone());
            }
        }

        let point_cfgs: Vec<PipelinePointConfig> = config
            .points
            .iter()
            .map(|point| {
                PipelinePointConfig::passthrough(
                    &point.point_id,
                    &point.point_id,
                    &point.device_id,
                    "",
                )
            })
            .collect();
        let pipeline = match DataProcessor::new(point_cfgs) {
            Ok(processor) => Some(StdMutex::new(AcquisitionPipeline::new(
                processor,
                None, // V1：配置无公式段；公式接线面已就绪（见模块文档）。
                None, // 入队背压走 NorthRuntime::submit（发送队列自带落盘降级）。
                Arc::new(AuditLog::new(64)),
                Arc::clone(&clock),
            ))),
            Err(err) => {
                error!(
                    error = %err,
                    "dataplane: [ERROR] point pipeline build failed; northbound forwarding                      stays disabled (capture and scheduling continue; fix [[points]] to recover)"
                );
                None
            }
        };

        let lanes: Vec<OutletLane> = config
            .outlets
            .iter()
            .map(|outlet| OutletLane {
                name: outlet.name.clone(),
                encoder: encoder_for(outlet.encoding.into()),
                seq: AtomicU64::new(0),
            })
            .collect();

        Self {
            inner,
            pipeline,
            lanes: StdMutex::new(lanes),
            runtime: OnceLock::new(),
            gateway_id,
            clock,
            counters: DataPlaneCounters::default(),
            live_tx,
            health,
            no_push,
            config: config_shared,
            rules: StdMutex::new(build_rule_engine(config)),
            rule_version: AtomicU64::new(rule_version),
            alarm_engine: StdMutex::new(build_alarm_engine(config)),
            alarm_version: AtomicU64::new(alarm_version),
            alarms,
        }
    }

    /// 转发规则引擎是否已装配（存在启用的合法 `[[rules]]`）。
    #[must_use]
    pub fn has_rules(&self) -> bool {
        lock_or_recover(&self.rules).is_some()
    }

    /// 告警引擎是否已装配（`[alarms]` 已启用且至少有一条可用规则）。
    #[must_use]
    pub fn has_alarms(&self) -> bool {
        lock_or_recover(&self.alarm_engine).rule_count() > 0
    }

    /// 配置版本变化 → 重建规则引擎（**管理面新增 / 启停 / 删除规则的热生效点**）。
    ///
    /// 建造仅当版本真的变了（比较原子版本号），热路径零重建、零分配。
    fn sync_rule_engine(&self) {
        let version = self.config.version();
        if version == self.rule_version.load(Ordering::Relaxed) {
            return;
        }
        let rules_count = build_rule_engine(&self.config.snapshot()).map(|engine| {
            let count = engine.rule_count();
            *lock_or_recover(&self.rules) = Some(engine);
            count
        });
        match rules_count {
            Some(count) => info!(
                config_version = version,
                rules = count,
                "dataplane: forwarding rule engine (re)built from [[rules]]"
            ),
            None => debug!(
                config_version = version,
                "dataplane: no enabled forwarding rule; rule stage is idle"
            ),
        }
        self.rule_version.store(version, Ordering::Relaxed);
    }

    /// 配置版本变化 → 重建告警引擎（**管理面改告警规则的热生效点**）。
    ///
    /// 与 [`Self::sync_rule_engine`] 同口径：版本没变直接返回；重建后清空各轨道
    /// 运行态（旧的「已持续时长 / 抑制窗口」对新规则没有意义）。
    fn sync_alarm_engine(&self) {
        let version = self.config.version();
        if version == self.alarm_version.load(Ordering::Relaxed) {
            return;
        }
        let mut engine = lock_or_recover(&self.alarm_engine);
        let rebuilt = build_alarm_engine(&self.config.snapshot());
        let count = rebuilt.rule_count();
        *engine = rebuilt;
        engine.reset_tracks();
        drop(engine);
        info!(
            config_version = version,
            rules = count,
            "dataplane: alarm engine (re)built from [alarms]"
        );
        self.alarm_version.store(version, Ordering::Relaxed);
    }

    /// 告警求值：对每个发射样本跑一遍阈值 + 去抖 + 抑制，返回本拍应当落库的
    /// 告警记录（新触发 / 续期 / 恢复）。
    ///
    /// 返回值**只是记录的事实**（计数 + 日志），不投递任何东西：告警的出口是
    /// 管理面列表，不是北向 topic。
    fn evaluate_alarms(&self, processed: &[ProcessedSample], now_ns: i64) -> Vec<AlarmRecord> {
        let mut engine = lock_or_recover(&self.alarm_engine);
        let mut fired: Vec<AlarmRecord> = Vec::new();
        for sample in processed {
            fired.extend(engine.evaluate(sample, now_ns));
        }
        drop(engine);
        DataPlaneCounters::add(&self.counters.alarms_evaluated, processed.len() as u64);
        DataPlaneCounters::add(&self.counters.alarms_fired, fired.len() as u64);
        fired
    }

    /// 规则求值：对每个发射样本跑一遍 WHERE + DO，返回 `DO` 产出的路由消息。
    ///
    /// 返回值**只做事实记账**（计数 + 日志）：北向泳道的 topic 由出口配置
    /// `topic_prefix` 固定（`NorthRuntime::submit` 无 topic 入参），规则里的
    /// `publish{topic}` 无法在现有北投递路径上保序投递——这里绝不假装已投递，
    /// 也不把规则消息塞进遥测批次污染对端 schema。
    fn evaluate_rules(&self, processed: &[ProcessedSample]) -> Vec<RoutedMessage> {
        let rules = lock_or_recover(&self.rules);
        let Some(engine) = rules.as_ref() else {
            return Vec::new();
        };
        let mut routed: Vec<RoutedMessage> = Vec::new();
        for sample in processed {
            routed.extend(engine.route(sample));
        }
        drop(rules);
        DataPlaneCounters::add(&self.counters.rules_evaluated, processed.len() as u64);
        DataPlaneCounters::add(&self.counters.rules_routed, routed.len() as u64);
        routed
    }

    /// 该点位是否被排除在北向转发批次之外（`push_enabled = false`）。
    ///
    /// **仅用于北向过滤**：实时遥测流不经过本判据（关推送的点位照常进实时流）。
    fn is_push_blocked(&self, device_id: &str, point_id: &str) -> bool {
        self.no_push
            .get(device_id)
            .is_some_and(|points| points.contains(point_id))
    }

    /// 挂载北向运行期句柄（晚绑定；幂等——重复 attach 忽略后者）。
    ///
    /// 北向启动期被拒绝（配置非法 / TLS 校验失败）的出口不产生泳道剔除之外的
    /// 任何处理：其 `error!` 已由 `NorthRuntime::start` 记录，这里仅摘除对应
    /// 泳道，避免每拍对未知出口反复报错。
    pub fn attach(&self, runtime: Arc<NorthRuntime>) {
        let started: HashSet<String> = runtime.outlet_names().into_iter().collect();
        let mut lanes = lock_or_recover(&self.lanes);
        let before = lanes.len();
        lanes.retain(|lane| started.contains(lane.name.as_str()));
        let detached = before - lanes.len();
        drop(lanes);
        if detached > 0 {
            warn!(
                detached,
                "dataplane: lanes for outlets rejected at north startup are detached"
            );
        }
        if self.runtime.set(runtime).is_err() {
            debug!("dataplane: attach called twice; keeping the first runtime handle");
        }
    }

    /// 北向运行期是否已挂载。
    #[must_use]
    pub fn is_attached(&self) -> bool {
        self.runtime.get().is_some()
    }

    /// 统计快照。
    #[must_use]
    pub fn stats(&self) -> DataPlaneStats {
        let c = &self.counters;
        DataPlaneStats {
            forward_cycles: DataPlaneCounters::get(&c.forward_cycles),
            pre_runtime_cycles: DataPlaneCounters::get(&c.pre_runtime_cycles),
            physical_emitted: DataPlaneCounters::get(&c.physical_emitted),
            derived_emitted: DataPlaneCounters::get(&c.derived_emitted),
            admitted: DataPlaneCounters::get(&c.admitted),
            spilled: DataPlaneCounters::get(&c.spilled),
            rejected: DataPlaneCounters::get(&c.rejected),
            encode_errors: DataPlaneCounters::get(&c.encode_errors),
            rules_evaluated: DataPlaneCounters::get(&c.rules_evaluated),
            rules_routed: DataPlaneCounters::get(&c.rules_routed),
            alarms_evaluated: DataPlaneCounters::get(&c.alarms_evaluated),
            alarms_fired: DataPlaneCounters::get(&c.alarms_fired),
        }
    }

    /// 把一轮采集样本经管线变换后投递到全部出口泳道（同步、不阻塞采集路径）。
    ///
    /// 失败语义：单点管线错误 / 单出口编码失败只影响该点 / 该出口（warn + 计数），
    /// 绝不中断采集、绝不 panic；发送队列硬上限拒绝时数据已由 submit 交还并审计，
    /// 这里只补计数与告警。
    fn forward(&self, samples: &[RawSample]) {
        if samples.is_empty() {
            return;
        }
        // 转发规则引擎：配置版本变了就重建（管理面新增 / 启停 / 删除规则的
        // 热生效点——与 `persist_config` 的 `ConfigShared::replace` 同.version())
        // 这道闸门保证「页面改的规则下一拍就生效」，无需重启网关。
        self.sync_rule_engine();
        let Some(pipeline) = &self.pipeline else {
            return; // 管线配置非法：只采不转（构造期已 error!，不逐拍刷屏）。
        };
        self.sync_alarm_engine();
        let want_alarms = self.has_alarms();
        let want_rules = self.has_rules();

        let ts = self.clock.now_ns();
        let mut points = Vec::with_capacity(samples.len());
        // 规则 / 告警求值需要的原始处理样本（仅当确实有启用规则或告警规则时
        // 收集，零规则零告警时零开销）。
        let mut eval_inputs: Vec<ProcessedSample> = Vec::new();
        let want_eval = want_rules || want_alarms;
        {
            let mut pipeline = lock_or_recover(pipeline);
            for sample in samples {
                match pipeline.ingest_physical(sample.clone(), ts) {
                    Ok(Some(processed)) => {
                        DataPlaneCounters::bump(&self.counters.physical_emitted);
                        if want_eval {
                            eval_inputs.push(processed.clone());
                        }
                        points.push(sample_to_data_point(&processed));
                    }
                    Ok(None) => {} // 死区过滤：不发射（公式输入已在 ingest 内记账）。
                    Err(err) => warn!(
                        point_id = %sample.source_id,
                        error = %err,
                        "dataplane: point pipeline rejected the sample; point skipped"
                    ),
                }
            }
            // 周期屏障：物理点全部可见后统一求值派生点（有公式配置时才产出）。
            for derived in pipeline.finish_cycle(ts) {
                if derived.failed {
                    continue; // 计算失败不伪装好值（质量语义由公式引擎承载）。
                }
                let Some(value) = derived.value else {
                    continue;
                };
                DataPlaneCounters::bump(&self.counters.derived_emitted);
                let derived_sample = ProcessedSample {
                    // 派生点无物理设备归属：以网关为 `device_id`（北向 schema 必填）。
                    device_id: self.gateway_id.clone(),
                    point_id: derived.point_id,
                    value,
                    unit: String::new(),
                    device_ts_ns: None,
                    collected_ts_ns: ts,
                    quality: derived.quality.to_wire(),
                };
                if want_eval {
                    eval_inputs.push(derived_sample.clone());
                }
                points.push(sample_to_data_point(&derived_sample));
            }
        }
        // 告警求值（真实样本 → 真实告警记录）：产生的是**记录**，不是投递——
        // 落进 `AlarmStore` 后由 `GET /api/alerts` 原样回显，与「是否有出口」无关。
        if want_alarms && !eval_inputs.is_empty() {
            let fired = self.evaluate_alarms(&eval_inputs, ts);
            if !fired.is_empty() {
                // 记录写入仓库：返回「最新在前」的快照，前端列表即此顺序。
                self.alarms.upsert(&fired);
            }
        }
        // 转发规则求值（WHERE + DO）：规则命中产出的消息只做事实记账（计数 +
        // debug 日志），**不假装投递**——北向泳道 topic 由出口配置固定，`publish`
        // 动作的 topic 在现有 `NorthRuntime::submit` 路径上无从投递（见
        // `Self::evaluate_rules` 注释）。
        if !eval_inputs.is_empty() {
            let routed = self.evaluate_rules(&eval_inputs);
            if !routed.is_empty() {
                debug!(
                    count = routed.len(),
                    "dataplane: forwarding rules produced routed messages (counted, not                      delivered to the outlet topic by design — see evaluate_rules)"
                );
            }
        }
        // 北向运行期未就绪（启动竞态窗口）或根本没有出口：样本照常产生、告警
        // **照常记录**（告警的出口是管理面列表，不是北向 topic），本轮只是不投递。
        // 这行必须在告警 / 规则求值**之后**——否则「没有出口的网关」永远不产生
        // 任何告警记录，页面上就是一直空着（本 ticket 要消掉的就是这个现象）。
        let Some(runtime) = self.runtime.get() else {
            DataPlaneCounters::bump(&self.counters.pre_runtime_cycles);
            return;
        };
        DataPlaneCounters::bump(&self.counters.forward_cycles);
        if points.is_empty() {
            return; // 全部被死区过滤 / 求值失败：本拍无北向批次。
        }
        let batch = TelemetryBatch {
            points,
            ts,
            gateway_id: self.gateway_id.clone(),
            auth: None, // 签名块由既有北向签名链路负责，数据面不重复实现。
        };

        // 实时遥测扇出（task 52）：把**全部**解码后逐点遥测广播给管理面
        // `/api/stream` 订阅者——含「关推送」的点位（需求 6：关推送仍进实时流）。
        // 复用 `JsonEncoder` 字段名、把 `value` 解码为 JSON 数值；仅 `Err`/`None`
        // 丢弃、不 panic（broadcast 无订阅者 / 编码异常均为正常路径，不影响北向投递）。
        if let Some(live) = LiveTelemetry::from_batch(&batch) {
            let _ = self.live_tx.send(live);
        }

        // 需求 6：北向转发批次**排除** `push_enabled = false` 的点位。
        // 过滤在此真实投递路径（而非接口层）执行；无被排除点位时零额外开销（直接搬移）。
        let any_blocked = batch
            .points
            .iter()
            .any(|p| self.is_push_blocked(&p.device_id, &p.point_id));
        let push_points: Vec<protocol_proto::DataPoint> = if any_blocked {
            batch
                .points
                .into_iter()
                .filter(|p| !self.is_push_blocked(&p.device_id, &p.point_id))
                .collect()
        } else {
            batch.points
        };
        if push_points.is_empty() {
            // 本拍点位全部关推送：照常采集、已进实时流，但本拍无北向批次。
            debug!(
                "dataplane: all emitted points have push disabled; no northbound batch this cycle"
            );
            return;
        }
        let push_batch = TelemetryBatch {
            points: push_points,
            ts,
            gateway_id: batch.gateway_id,
            auth: None,
        };

        let lanes = lock_or_recover(&self.lanes);
        for lane in lanes.iter() {
            let payload = match lane.encoder.encode_batch(&push_batch) {
                Ok(payload) => payload,
                Err(err) => {
                    DataPlaneCounters::bump(&self.counters.encode_errors);
                    warn!(
                        outlet = %lane.name,
                        error = %err,
                        "dataplane: batch encode failed for the outlet; outlet skipped this cycle"
                    );
                    continue;
                }
            };
            // 序号分配：本泳道内单调递增（1 起）；溢出由 fetch_add 自然回绕，
            // 发送队列只把 seq 用作批次标识（幂等去重在补发路径按既有机制执行）。
            let seq = lane.seq.fetch_add(1, Ordering::Relaxed) + 1;
            match runtime.submit(&lane.name, seq, payload) {
                Ok(PushOutcome::Admitted) => {
                    DataPlaneCounters::bump(&self.counters.admitted);
                }
                Ok(PushOutcome::Spilled { count }) => {
                    DataPlaneCounters::add(&self.counters.spilled, count as u64);
                    debug!(
                        outlet = %lane.name,
                        count,
                        "dataplane: send queue high-water exceeded; batch spilled to queue.db"
                    );
                }
                Ok(PushOutcome::Rejected { returned }) => {
                    DataPlaneCounters::add(&self.counters.rejected, returned.len() as u64);
                    warn!(
                        outlet = %lane.name,
                        count = returned.len(),
                        "dataplane: [WARN] send queue hard limit reached and spill failed; \
                         batch returned to caller (never silently dropped)"
                    );
                }
                Err(err) => warn!(
                    outlet = %lane.name,
                    error = %err,
                    "dataplane: submit rejected by the north runtime; outlet skipped this cycle"
                ),
            }
        }
    }
}

// ---- task 52：实时遥测帧（管理面 `/api/stream` 广播载荷） ----

/// 单点实时遥测（与北向 JSON 信封同构，但 `value` 已解码为 JSON 数值以便前端直显）。
#[derive(Debug, Clone, Serialize)]
pub struct LivePoint {
    /// 设备标识（北向 schema 必填）。
    pub device_id: String,
    /// 点位标识。
    pub point_id: String,
    /// 解码后的数值（f64 小端解码为 JSON number；非有限浮点为 `null`）。
    pub value: Value,
    /// 单位（工程单位字符串，可能为空）。
    pub unit: String,
    /// 采集时刻（纳秒，**字符串编码**，大数红线）。
    pub ts: String,
    /// 质量码英文枚举名（北向契约：`GOOD` / `UNCERTAIN` / `BAD` / …）。
    pub quality: String,
    /// 质量码整数值（JSON number；范围安全，走数值）。
    pub quality_code: Value,
}

/// 实时遥测帧（task 52）：与北向 `JsonEncoder` 信封同构（字段名 / 结构一致），
/// 仅 `value` 由 `{t:"f64le",b64}` 解码为 JSON 数值。
///
/// 作为 `broadcast` 广播项（要求 `Clone`）；管理面 `/api/stream` 直接序列化本类型
/// 为 SSE `data:` 帧，前端仪表盘无需再做 f64le 解码。
#[derive(Debug, Clone, Serialize)]
pub struct LiveTelemetry {
    /// 编码标识（恒 `"json"`）。
    pub enc: String,
    /// 本批次点位数组（已解码数值）。
    pub points: Vec<LivePoint>,
    /// 批次时间戳（纳秒，**字符串编码**，大数红线）。
    pub ts: String,
    /// 网关标识。
    pub gateway_id: String,
    /// 签名块（北向无签名时为 `null`）。
    pub auth: Option<Value>,
}

impl LiveTelemetry {
    /// 由北向 [`TelemetryBatch`] 构造实时帧（复用 `JsonEncoder` 字段名）。
    ///
    /// 失败（编码 / JSON 解析异常）返回 `None`——调用方（数据面热路径）仅丢弃该帧、
    /// 不 panic、不影响北向投递。
    #[must_use]
    pub fn from_batch(batch: &TelemetryBatch) -> Option<Self> {
        let encoded = JsonEncoder.encode_batch(batch).ok()?;
        let root: Value = serde_json::from_slice(&encoded).ok()?;
        let points: Vec<LivePoint> = root
            .get("points")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .map(|point| LivePoint {
                        device_id: str_field(point, "device_id"),
                        point_id: str_field(point, "point_id"),
                        value: decode_value(point),
                        unit: str_field(point, "unit"),
                        ts: str_field(point, "ts"),
                        quality: str_field(point, "quality"),
                        quality_code: point.get("quality_code").cloned().unwrap_or(Value::Null),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(LiveTelemetry {
            enc: root
                .get("enc")
                .and_then(Value::as_str)
                .unwrap_or("json")
                .to_string(),
            points,
            ts: str_field(&root, "ts"),
            gateway_id: str_field(&root, "gateway_id"),
            auth: root.get("auth").cloned(),
        })
    }
}

/// 取 `Value` 对象字符串字段（缺省空串）。
fn str_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// 把北向 `value` 字段（`{t:"f64le",b64}` 或 `null`/数值）解码为 JSON 数值。
///
/// - 8 字节 f64le → 解码为 JSON number（前端直接展示）；
/// - 非有限浮点（`value:null`）→ 保留 `null`；
/// - 其余（已是数值 / blob）→ 原样保留。
fn decode_value(point: &Value) -> Value {
    match point.get("value") {
        Some(Value::Object(_)) => {
            let Some(b64) = point
                .get("value")
                .and_then(|v| v.get("b64"))
                .and_then(Value::as_str)
            else {
                return Value::Null;
            };
            match base64::engine::general_purpose::STANDARD.decode(b64) {
                Ok(bytes) if bytes.len() == 8 => {
                    let mut arr = [0u8; 8];
                    arr.copy_from_slice(&bytes);
                    Value::from(f64::from_le_bytes(arr))
                }
                _ => Value::Null,
            }
        }
        Some(other) => other.clone(),
        None => Value::Null,
    }
}

#[async_trait]
impl PollHandler for NorthDataPlane {
    async fn poll(&self, group: &str, point_ids: &[String]) -> DaemonResult<Vec<RawSample>> {
        // 先采集（南向读 + 解码），再把样本变换 / 投递，最后原样返回样本供
        // 调度器统计口径（len）使用——样本的产生不依赖北向是否就绪。
        //
        // 需求 1：本拍真实结果（成功 / 失败）落健康度注册表——`group` 即 device_id
        //（bootstrap `build_groups` 一组一设备），管理面据此计算三态。失败原因仍原样
        // 上抛（调度器 / 看门狗负责），健康度只做事实记账、不吞错。
        let result = self.inner.poll(group, point_ids).await;
        let ts = now_ms();
        match &result {
            Ok(_) => self.health.record_success(group, ts),
            Err(_) => self.health.record_failure(group, ts),
        }
        let samples = result?;
        self.forward(&samples);
        Ok(samples)
    }

    async fn refresh_devices(&self, config: &GatewayConfig) {
        // 转发给内层真实采集动作：南向的设备计划表必须跟着热重载换，否则调度器
        // 侧重建后新组每拍都撞 `unknown device group`。
        self.inner.refresh_devices(config).await;
    }
}

// ---------------------------------------------------------------------------
// 单元测试（假南向 handler + 真实 NorthRuntime / 发送队列链路）
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GatewayConfig, OutletEncoding};
    use crate::north::encoder::{decode_batch, Encoding};
    use crate::north::mqtt::AuditSink;
    use crate::north::runtime::NorthRuntimeConfig;
    use crate::offline_queue::{OfflineQueue, QueueConfig};

    use std::time::Duration;
    use tokio::sync::broadcast;

    /// 测试内层 handler：每次返回 `n` 个值 = 12.5 的样本。
    struct FakeInner {
        n: usize,
    }

    #[async_trait]
    impl PollHandler for FakeInner {
        async fn poll(&self, group: &str, point_ids: &[String]) -> DaemonResult<Vec<RawSample>> {
            let _ = group;
            Ok(point_ids
                .iter()
                .take(self.n)
                .map(|point_id| RawSample {
                    source_id: point_id.clone(),
                    value: 12.5,
                    quality: protocol_proto::Quality::Good,
                    device_ts_ns: None,
                })
                .collect())
        }
    }

    /// 测试用健康度注册表（需求 1 落账点可观测）。
    fn test_health() -> Arc<DeviceHealthRegistry> {
        Arc::new(DeviceHealthRegistry::new())
    }

    /// 丢弃型审计出口（单测不关心背压审计内容）。
    struct DropSink;

    impl AuditSink for DropSink {
        fn emit(&self, _events: Vec<crate::backpressure::BackpressureAudit>) {}
    }

    /// 带一个 outlet + 一个 modbus 点位的最小配置（TOML 文本）。
    fn config_with(outlet_broker: &str, encoding: &str) -> GatewayConfig {
        GatewayConfig::parse(&format!(
            "[gateway]\ngateway_id = \"gw-dp\"\n\n\
             [[outlets]]\nname = \"north-1\"\nbroker = {outlet_broker:?}\nqos = 1\n\
             encoding = {encoding:?}\n\n\
             [[points]]\ndevice_id = \"dev-01\"\npoint_id = \"p1\"\n\
             protocol = \"modbus-tcp\"\naddress = \"127.0.0.1:502\"\nfrequency_ms = 100\n"
        ))
        .expect("parse config")
    }

    /// 在临时目录打开共享 OfflineQueue 并启动一个单出口 NorthRuntime。
    fn start_runtime(dir: &std::path::Path) -> Arc<NorthRuntime> {
        let queue = Arc::new(
            OfflineQueue::open(
                QueueConfig::new(dir.join("queue.db"), "gw-dp").expect("queue cfg"),
                Arc::new(crate::offline_queue::SystemClock),
            )
            .expect("open queue"),
        );
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let outlet = config_with("mqtt://127.0.0.1:1883", "protobuf")
            .outlets
            .remove(0);
        let runtime = Arc::new(NorthRuntime::start(
            "gw-dp",
            &[outlet],
            NorthRuntimeConfig::new(
                queue,
                Arc::new(crate::offline_queue::SystemClock),
                Arc::new(DropSink),
            )
            .with_tick(Duration::from_millis(50)),
            shutdown_rx,
        ));
        // 保活停机信号发送端（驱动任务只读 watch；drop sender 不会关断循环，
        // 但显式保活语义更清晰）。
        std::mem::forget(shutdown_tx);
        runtime
    }

    /// 快速路径：采集样本经管线 → protobuf 批次 → 发送队列在途 1 条，
    /// payload 可解码回已知值（12.5）与点位 / 设备标识。
    #[tokio::test]
    async fn poll_forwards_samples_into_send_queue() {
        let dir = tempfile::tempdir().expect("tempdir");
        let runtime = start_runtime(dir.path());
        let config = config_with("mqtt://127.0.0.1:1883", "protobuf");
        let (live_tx, mut live_rx) = broadcast::channel(16);
        let plane = NorthDataPlane::new(
            Arc::new(FakeInner { n: 1 }),
            &config,
            Arc::new(ConfigShared::new(Default::default())),
            live_tx,
            test_health(),
            AlarmStore::shared(),
        );
        assert!(!plane.is_attached());
        plane.attach(Arc::clone(&runtime));
        assert!(plane.is_attached());

        let samples = plane
            .poll("dev-01", &["p1".to_string()])
            .await
            .expect("poll");
        assert_eq!(samples.len(), 1, "samples must pass through untouched");
        assert_eq!(samples[0].value, 12.5);

        let stats = plane.stats();
        assert_eq!(stats.forward_cycles, 1);
        assert_eq!(stats.physical_emitted, 1);
        assert_eq!(stats.admitted, 1, "one batch admitted into the send queue");

        let outlet = runtime.outlet("north-1").expect("outlet started");
        assert_eq!(outlet.send().pending(), 1, "batch waits in the send queue");
        let ready = outlet.send().take_ready(8);
        assert_eq!(ready.len(), 1);
        let batch = decode_batch(Encoding::Protobuf, &ready[0].payload).expect("decode");
        assert_eq!(batch.gateway_id, "gw-dp");
        assert_eq!(batch.points.len(), 1);
        assert_eq!(batch.points[0].device_id, "dev-01");
        assert_eq!(batch.points[0].point_id, "p1");
        let value = f64::from_le_bytes(batch.points[0].value.as_slice().try_into().expect("8B"));
        assert!((value - 12.5).abs() < 1e-9, "decoded value must be 12.5");

        // 实时遥测广播（task 52）：解码后逐点遥测扇出给 /api/stream 订阅者。
        let live = live_rx
            .try_recv()
            .expect("live telemetry frame must be broadcast");
        assert_eq!(live.points.len(), 1, "one point in the frame");
        assert_eq!(live.points[0].device_id, "dev-01");
        assert_eq!(live.points[0].point_id, "p1");
        assert!(
            (live.points[0].value.as_f64().expect("value is number") - 12.5).abs() < 1e-9,
            "value decoded from f64le"
        );
        assert!(
            !live.points[0].ts.is_empty() && live.points[0].ts.bytes().all(|b| b.is_ascii_digit()),
            "ts must be string-encoded nanos (大数红线)"
        );
        assert_eq!(live.points[0].quality, "GOOD", "north quality contract");
    }

    /// 北向运行期未 attach（启动竞态窗口）：样本照常返回、不投递、不 panic。
    #[tokio::test]
    async fn poll_before_attach_keeps_samples_without_forwarding() {
        let dir = tempfile::tempdir().expect("tempdir");
        let runtime = start_runtime(dir.path());
        let config = config_with("mqtt://127.0.0.1:1883", "protobuf");
        let (live_tx, _) = broadcast::channel(8);
        let plane = NorthDataPlane::new(
            Arc::new(FakeInner { n: 1 }),
            &config,
            Arc::new(ConfigShared::new(Default::default())),
            live_tx,
            test_health(),
            AlarmStore::shared(),
        );

        let samples = plane
            .poll("dev-01", &["p1".to_string()])
            .await
            .expect("poll");
        assert_eq!(samples.len(), 1);
        assert_eq!(plane.stats().pre_runtime_cycles, 1);
        assert_eq!(plane.stats().forward_cycles, 0);
        assert_eq!(
            runtime.outlet("north-1").expect("outlet").send().pending(),
            0,
            "nothing forwarded before attach"
        );
    }

    /// 无 `[[outlets]]`：泳道为空 → 采样照常、投递为无操作（既有行为保持）。
    #[tokio::test]
    async fn no_outlets_is_a_no_op() {
        let config = GatewayConfig::parse(
            "[gateway]\ngateway_id = \"gw-dp\"\n\n\
             [[points]]\ndevice_id = \"dev-01\"\npoint_id = \"p1\"\n\
             protocol = \"modbus-tcp\"\naddress = \"127.0.0.1:502\"\nfrequency_ms = 100\n",
        )
        .expect("parse");
        assert!(config.outlets.is_empty());
        let (live_tx, _) = broadcast::channel(8);
        let plane = NorthDataPlane::new(
            Arc::new(FakeInner { n: 1 }),
            &config,
            Arc::new(ConfigShared::new(Default::default())),
            live_tx,
            test_health(),
            AlarmStore::shared(),
        );
        let samples = plane
            .poll("dev-01", &["p1".to_string()])
            .await
            .expect("poll");
        assert_eq!(samples.len(), 1);
        // 运行期未 attach（无 outlets 场景在生产装配中本就不会创建运行期）：
        // 采样照常、投递为无操作（计入 pre_runtime 窗口）。
        let stats = plane.stats();
        assert_eq!(stats.pre_runtime_cycles, 1);
        assert_eq!(stats.forward_cycles, 0);
        assert_eq!(stats.admitted, 0, "no lanes → nothing to submit");
    }

    /// 出口编码按 `[[outlets]].encoding` 独立选择：json 出口产出的 payload
    /// 外层带 `enc: "json"`（与 protobuf 出口字节不同、语义一致）。
    #[tokio::test]
    async fn per_outlet_encoding_is_honored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = Arc::new(
            OfflineQueue::open(
                QueueConfig::new(dir.path().join("queue.db"), "gw-dp").expect("queue cfg"),
                Arc::new(crate::offline_queue::SystemClock),
            )
            .expect("open queue"),
        );
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        std::mem::forget(shutdown_tx);
        let mut outlets = config_with("mqtt://127.0.0.1:1883", "json").outlets;
        outlets[0].encoding = OutletEncoding::Json;
        let runtime = Arc::new(NorthRuntime::start(
            "gw-dp",
            &outlets,
            NorthRuntimeConfig::new(
                queue,
                Arc::new(crate::offline_queue::SystemClock),
                Arc::new(DropSink),
            )
            .with_tick(Duration::from_millis(50)),
            shutdown_rx,
        ));
        let (live_tx, _) = broadcast::channel(8);
        let plane = NorthDataPlane::new(
            Arc::new(FakeInner { n: 1 }),
            &config_with("x", "json"),
            Arc::new(ConfigShared::new(Default::default())),
            live_tx,
            test_health(),
            AlarmStore::shared(),
        );
        plane.attach(Arc::clone(&runtime));
        plane
            .poll("dev-01", &["p1".to_string()])
            .await
            .expect("poll");

        let outlet = runtime.outlet("north-1").expect("outlet");
        let ready = outlet.send().take_ready(8);
        assert_eq!(ready.len(), 1);
        let text = String::from_utf8(ready[0].payload.clone()).expect("json payload is utf-8");
        assert!(
            text.contains("\"enc\":\"json\""),
            "json encoding honored: {text}"
        );
        let batch = decode_batch(Encoding::Json, &ready[0].payload).expect("decode json");
        assert_eq!(batch.points[0].point_id, "p1");
    }

    /// 北向启动期被拒绝的出口：attach 时泳道被摘除，后续投递不报错。
    #[tokio::test]
    async fn attach_detaches_lanes_for_rejected_outlets() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = Arc::new(
            OfflineQueue::open(
                QueueConfig::new(dir.path().join("queue.db"), "gw-dp").expect("queue cfg"),
                Arc::new(crate::offline_queue::SystemClock),
            )
            .expect("open queue"),
        );
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        std::mem::forget(shutdown_tx);
        // TLS 出口缺 CA 文件 → NorthRuntime::start 拒绝该出口（skipped）。
        let mut outlets = config_with("mqtts://127.0.0.1:8883", "protobuf").outlets;
        outlets[0].ca_cert_path = Some("/definitely/not/here/ca.crt".to_string());
        let runtime = Arc::new(NorthRuntime::start(
            "gw-dp",
            &outlets,
            NorthRuntimeConfig::new(
                queue,
                Arc::new(crate::offline_queue::SystemClock),
                Arc::new(DropSink),
            ),
            shutdown_rx,
        ));
        assert_eq!(runtime.outlet_count(), 0, "outlet rejected at startup");

        let (live_tx, _) = broadcast::channel(8);
        let plane = NorthDataPlane::new(
            Arc::new(FakeInner { n: 1 }),
            &config_with("mqtts://127.0.0.1:8883", "protobuf"),
            Arc::new(ConfigShared::new(Default::default())),
            live_tx,
            test_health(),
            AlarmStore::shared(),
        );
        plane.attach(Arc::clone(&runtime));
        let samples = plane
            .poll("dev-01", &["p1".to_string()])
            .await
            .expect("poll");
        assert_eq!(samples.len(), 1, "capture unaffected by outlet rejection");
        let stats = plane.stats();
        assert_eq!(stats.forward_cycles, 1);
        assert_eq!(stats.admitted, 0, "detached lane submits nothing");
        assert_eq!(stats.encode_errors, 0, "no per-cycle error spam");
    }

    /// QA（task 52）：`LiveTelemetry` 序列化——字段名与北向信封一致、`value` 解码为
    /// 数值、大数 `ts` 走字符串、非有限浮点 `value` 为 `null`。
    #[test]
    fn live_telemetry_serialization_matches_envelope() {
        let batch = TelemetryBatch {
            points: vec![protocol_proto::DataPoint {
                device_id: "dev-01".to_string(),
                point_id: "p1".to_string(),
                value: 12.5f64.to_le_bytes().to_vec(),
                unit: "degC".to_string(),
                ts: 1_700_000_000_000_000_000i64,
                quality: protocol_proto::Quality::Good as i32,
            }],
            ts: 1_700_000_000_000_000_000i64,
            gateway_id: "gw-test".to_string(),
            auth: None,
        };
        let live = LiveTelemetry::from_batch(&batch).expect("live frame");
        let json = serde_json::to_string(&live).expect("serialize");
        let value: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(value["enc"], "json");
        assert_eq!(value["gateway_id"], "gw-test");
        assert_eq!(value["points"][0]["device_id"], "dev-01");
        assert_eq!(value["points"][0]["point_id"], "p1");
        assert!(
            (value["points"][0]["value"].as_f64().expect("value number") - 12.5).abs() < 1e-9,
            "value must be decoded number"
        );
        assert!(
            value["points"][0]["ts"].is_string(),
            "ts must be string (大数红线)"
        );
        assert_eq!(
            value["points"][0]["quality"], "GOOD",
            "north quality contract"
        );
        assert_eq!(value["auth"], serde_json::Value::Null);
    }

    // ---- 需求 1 / 6：推送开关北向过滤 + 真实健康度落账 ----

    /// 两点的配置（p1 推送、p2 `push=false`）。
    fn config_two_points_one_disabled() -> GatewayConfig {
        GatewayConfig::parse(
            "[gateway]\ngateway_id = \"gw-dp\"\n\n\
             [[outlets]]\nname = \"north-1\"\nbroker = \"mqtt://127.0.0.1:1883\"\nqos = 1\n\
             encoding = \"protobuf\"\n\n\
             [[points]]\ndevice_id = \"dev-01\"\npoint_id = \"p1\"\nprotocol = \"modbus-tcp\"\n\
             address = \"127.0.0.1:502\"\nfrequency_ms = 100\n\n\
             [[points]]\ndevice_id = \"dev-01\"\npoint_id = \"p2\"\nprotocol = \"modbus-tcp\"\n\
             address = \"127.0.0.1:502\"\nfrequency_ms = 100\npush = false\n",
        )
        .expect("parse two-point config")
    }

    /// 需求 6：`push_enabled = false` 的点位**不进北向转发批次**，
    /// 但**仍在实时流**（`/api/stream` 广播）中；同批次的推送点位照常投递。
    #[tokio::test]
    async fn push_disabled_point_stays_in_live_stream_but_not_northbound() {
        let dir = tempfile::tempdir().expect("tempdir");
        let runtime = start_runtime(dir.path());
        let config = config_two_points_one_disabled();
        let (live_tx, mut live_rx) = broadcast::channel(16);
        let health = test_health();
        let plane = NorthDataPlane::new(
            Arc::new(FakeInner { n: 2 }),
            &config,
            Arc::new(ConfigShared::new(Default::default())),
            live_tx,
            Arc::clone(&health),
            AlarmStore::shared(),
        );
        plane.attach(Arc::clone(&runtime));

        let samples = plane
            .poll("dev-01", &["p1".to_string(), "p2".to_string()])
            .await
            .expect("poll");
        assert_eq!(samples.len(), 2, "capture unaffected by the push flag");

        // 实时流：两点都在（关推送仍进实时流）。
        let live = live_rx.try_recv().expect("live telemetry frame");
        let ids: Vec<&str> = live.points.iter().map(|p| p.point_id.as_str()).collect();
        assert!(
            ids.contains(&"p1") && ids.contains(&"p2"),
            "both points must be present in the live frame: {ids:?}"
        );

        // 北向批次：仅推送点位 p1（关推送的 p2 被排除）。
        let outlet = runtime.outlet("north-1").expect("outlet");
        let ready = outlet.send().take_ready(8);
        assert_eq!(ready.len(), 1, "exactly one northbound batch");
        let batch = decode_batch(Encoding::Protobuf, &ready[0].payload).expect("decode");
        assert_eq!(
            batch.points.len(),
            1,
            "push-disabled point must be excluded from the northbound batch"
        );
        assert_eq!(batch.points[0].point_id, "p1");

        // 需求 1：成功轮询落健康度账。
        let snapshot = health.snapshot("dev-01").expect("health recorded");
        assert_eq!(snapshot.polls, 1);
        assert_eq!(snapshot.errors, 0);
        assert!(snapshot.last_success_ms.is_some(), "success timestamp set");
    }

    /// 需求 6 边界：全部点位关推送 → 本拍**无北向批次**（发送队列零投递），
    /// 但实时流照常产出、采集样本数量不变。
    #[tokio::test]
    async fn all_points_push_disabled_produces_no_northbound_batch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let runtime = start_runtime(dir.path());
        let config = GatewayConfig::parse(
            "[gateway]\ngateway_id = \"gw-dp\"\n\n\
             [[outlets]]\nname = \"north-1\"\nbroker = \"mqtt://127.0.0.1:1883\"\nqos = 1\n\
             encoding = \"protobuf\"\n\n\
             [[points]]\ndevice_id = \"dev-01\"\npoint_id = \"p1\"\nprotocol = \"modbus-tcp\"\n\
             address = \"127.0.0.1:502\"\nfrequency_ms = 100\npush = false\n",
        )
        .expect("parse all-disabled config");
        let (live_tx, mut live_rx) = broadcast::channel(16);
        let plane = NorthDataPlane::new(
            Arc::new(FakeInner { n: 1 }),
            &config,
            Arc::new(ConfigShared::new(Default::default())),
            live_tx,
            test_health(),
            AlarmStore::shared(),
        );
        plane.attach(Arc::clone(&runtime));

        let samples = plane
            .poll("dev-01", &["p1".to_string()])
            .await
            .expect("poll");
        assert_eq!(samples.len(), 1, "capture continues");

        let live = live_rx.try_recv().expect("live telemetry frame");
        assert_eq!(live.points.len(), 1, "live stream still carries the point");

        let outlet = runtime.outlet("north-1").expect("outlet");
        assert_eq!(
            outlet.send().pending(),
            0,
            "no northbound batch when every point has push disabled"
        );
        assert_eq!(plane.stats().admitted, 0);
    }

    /// 需求 1：南向读失败 → 健康度记失败（连续失败 +1、无成功时刻）。
    #[tokio::test]
    async fn poll_failure_is_recorded_in_health_registry() {
        struct FailingInner;
        #[async_trait]
        impl PollHandler for FailingInner {
            async fn poll(
                &self,
                _group: &str,
                _point_ids: &[String],
            ) -> DaemonResult<Vec<RawSample>> {
                Err(crate::error::DaemonError::ConfigError(
                    "simulated southbound failure".to_string(),
                ))
            }
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let runtime = start_runtime(dir.path());
        let config = config_with("mqtt://127.0.0.1:1883", "protobuf");
        let (live_tx, _) = broadcast::channel(8);
        let health = test_health();
        let plane = NorthDataPlane::new(
            Arc::new(FailingInner),
            &config,
            Arc::new(ConfigShared::new(config.clone())),
            live_tx,
            Arc::clone(&health),
            AlarmStore::shared(),
        );
        plane.attach(runtime);
        let err = plane
            .poll("dev-01", &["p1".to_string()])
            .await
            .expect_err("southbound failure must propagate");
        assert!(matches!(err, crate::error::DaemonError::ConfigError(_)));

        let snapshot = health.snapshot("dev-01").expect("failure recorded");
        assert_eq!(snapshot.polls, 1);
        assert_eq!(snapshot.errors, 1);
        assert_eq!(snapshot.consecutive_failures, 1);
        assert_eq!(
            snapshot.last_success_ms, None,
            "never succeeded → never-sampled"
        );
    }

    // ---- 需求：转发规则引擎接生产（[[rules]] → 数据面求值） ----

    /// 带 `[[rules]]` 的最小配置（规则体可注入）。
    fn config_with_rule(rule_body: &str) -> GatewayConfig {
        GatewayConfig::parse(&format!(
            "[gateway]
gateway_id = \"gw-dp\"

             [[outlets]]
name = \"north-1\"
broker = \"mqtt://127.0.0.1:1883\"
qos = 1
             encoding = \"protobuf\"

             [[points]]
device_id = \"dev-01\"
point_id = \"p1\"
protocol = \"modbus-tcp\"
             address = \"127.0.0.1:502\"
frequency_ms = 100

             [[rules]]
{}
",
            rule_body
        ))
        .expect("parse config with rule")
    }

    /// 一条合法启用规则（`value > 10` → publish）。
    fn enabled_rule_body(when_value: &str) -> String {
        format!(
            "id = \"r1\"
name = \"高温转发\"
enabled = true
priority = 1
             [rules.when]
  kind = \"cmp\"
  field = \"value\"
  op = \"gt\"
  value = {}
             [[rules.actions]]
kind = \"publish\"
topic = \"telemetry/high\"
",
            when_value
        )
    }

    /// QA（#17 BE-RULES）：启用的 `[[rules]]` 必须真的建出规则引擎（数据面可执行）；
    /// 全部禁用 / 无规则 → 引擎不装配，规则阶段静默空转（不假装生效）。
    #[test]
    fn enabled_rules_build_the_engine_and_disabled_ones_do_not() {
        let config = config_with_rule(&enabled_rule_body("10"));
        let shared = Arc::new(ConfigShared::new(config.clone()));
        let plane = NorthDataPlane::new(
            Arc::new(FakeInner { n: 1 }),
            &config,
            Arc::clone(&shared),
            broadcast::channel(8).0,
            test_health(),
            AlarmStore::shared(),
        );
        assert!(plane.has_rules(), "an enabled rule must build the engine");

        // 只禁用 → 引擎不装配（数据面不承接规则求值）。
        let mut disabled = config.clone();
        for rule in &mut disabled.rules {
            rule.enabled = false;
        }
        let shared = Arc::new(ConfigShared::new(disabled.clone()));
        let plane = NorthDataPlane::new(
            Arc::new(FakeInner { n: 1 }),
            &disabled,
            Arc::clone(&shared),
            broadcast::channel(8).0,
            test_health(),
            AlarmStore::shared(),
        );
        assert!(!plane.has_rules(), "no enabled rule → rule stage idle");
    }

    /// QA（#17 BE-RULES）：配置版本变化 → 规则引擎原地重建（页面改规则无需重启即生效）。
    #[test]
    fn rule_engine_is_rebuilt_when_the_config_version_moves() {
        let config = config_with("mqtt://127.0.0.1:1883", "protobuf");
        let shared = Arc::new(ConfigShared::new(config.clone()));
        let plane = NorthDataPlane::new(
            Arc::new(FakeInner { n: 1 }),
            &config,
            Arc::clone(&shared),
            broadcast::channel(8).0,
            test_health(),
            AlarmStore::shared(),
        );
        assert!(!plane.has_rules(), "no rule in the initial snapshot");

        let mut next = config.clone();
        next.rules.push(crate::config::RuleConfig {
            id: "r-live".to_string(),
            name: "热生效规则".to_string(),
            enabled: true,
            ..Default::default()
        });
        // 管理面写路径落盘后即刻推新快照 + 新版本号。
        let version = shared.replace(next.clone());
        assert!(version > 1, "replace must bump the config version");
        plane.sync_rule_engine();
        assert!(
            plane.has_rules(),
            "engine must be rebuilt after the version bump"
        );
    }

    /// QA（#17 BE-RULES）：规则集语义非法 → **降级为不跑规则**（不是跑一版半截的规则）。
    ///
    /// 与「拒绝落盘」是两道闸：管理面拦住落盘（400），落盘前提下（如手工改
    /// config.toml）数据面宁可空跑也不用一个非法规则集去转发全量样本。
    #[test]
    fn semantically_invalid_rule_set_degrades_the_rule_stage() {
        let rule_body = "id = \"r-bad\"
name = \"非法字段\"
enabled = true
                         [rules.when]

  kind = \"cmp\"
  field = \"bogus_field\"
  op = \"gt\"
  value = 1

                         [[rules.actions]]

kind = \"publish\"
topic = \"t/1\"
";
        let config = config_with_rule(rule_body);
        assert!(
            build_rule_engine(&config).is_none(),
            "invalid rule set must not build"
        );
        let shared = Arc::new(ConfigShared::new(config.clone()));
        let plane = NorthDataPlane::new(
            Arc::new(FakeInner { n: 1 }),
            &config,
            Arc::clone(&shared),
            broadcast::channel(8).0,
            test_health(),
            AlarmStore::shared(),
        );
        assert!(
            !plane.has_rules(),
            "degraded to idle instead of forwarding everything"
        );
    }

    /// QA（#17 BE-RULES）：WHERE 真的拿 payload 求值——`value > 10` 命中（routed > 0），
    /// `value > 100` 不命中（routed = 0）；两种情况下 `rules_evaluated` 都如实累加。
    #[tokio::test]
    async fn where_clause_is_evaluated_against_the_payload() {
        for (when_value, want_routed) in [("10", 1), ("100", 0)] {
            let dir = tempfile::tempdir().expect("tempdir");
            let runtime = start_runtime(dir.path());
            let config = config_with_rule(&enabled_rule_body(when_value));
            let shared = Arc::new(ConfigShared::new(config.clone()));
            let (live_tx, _) = broadcast::channel(8);
            let plane = NorthDataPlane::new(
                Arc::new(FakeInner { n: 1 }),
                &config,
                Arc::clone(&shared),
                live_tx,
                test_health(),
                AlarmStore::shared(),
            );
            plane.attach(Arc::clone(&runtime));

            let samples = plane
                .poll("dev-01", &["p1".to_string()])
                .await
                .expect("poll");
            assert_eq!(samples.len(), 1, "capture unaffected by the rule stage");

            let stats = plane.stats();
            assert!(
                stats.rules_evaluated >= 1,
                "every emitted sample must be offered to the rule stage (when={when_value})"
            );
            assert_eq!(
                stats.rules_routed, want_routed,
                "WHERE mis-evaluation (when={when_value})"
            );
        }
    }

    // ---- QA（#18 BE-ALARM）：告警引擎接生产（[alarms] → 数据面求值 → 记录） ----

    /// 带 `[alarms]` 的最小配置（`condition` 走前端在用的表达式写法）。
    fn config_with_alarm(alarm_body: &str) -> GatewayConfig {
        GatewayConfig::parse(&format!(
            "[gateway]
gateway_id = \"gw-dp\"

             [[outlets]]
name = \"north-1\"
broker = \"mqtt://127.0.0.1:1883\"
qos = 1
             encoding = \"protobuf\"

             [[points]]
device_id = \"dev-01\"
point_id = \"p1\"
protocol = \"modbus-tcp\"
             address = \"127.0.0.1:502\"
frequency_ms = 100

[alarms]
enabled = true
{}
",
            alarm_body
        ))
        .expect("parse config with alarms")
    }

    fn alarm_rule_body(condition: &str) -> String {
        format!(
            "[[alarms.rules]]
id = \"a1\"
name = \"超温告警\"
enabled = true
condition = \"{}\"
",
            condition
        )
    }

    /// 启用的 `[alarms]` 必须真的建出引擎；`enabled = false` / 段缺省 → 空装配
    /// （数据面整拍跳过告警阶段，不空跑求值）。
    #[test]
    fn enabled_alarms_build_the_engine_and_disabled_ones_do_not() {
        let config = config_with_alarm(&alarm_rule_body("[p1] > 240"));
        let shared = Arc::new(ConfigShared::new(config.clone()));
        assert!(
            NorthDataPlane::new(
                Arc::new(FakeInner { n: 1 }),
                &config,
                Arc::clone(&shared),
                broadcast::channel(8).0,
                test_health(),
                AlarmStore::shared(),
            )
            .has_alarms(),
            "an enabled alarm rule must build the engine"
        );

        let mut disabled = config.clone();
        disabled.alarms.as_mut().expect("section").enabled = false;
        let plane = NorthDataPlane::new(
            Arc::new(FakeInner { n: 1 }),
            &disabled,
            Arc::new(ConfigShared::new(disabled.clone())),
            broadcast::channel(8).0,
            test_health(),
            AlarmStore::shared(),
        );
        assert!(!plane.has_alarms(), "alarms disabled → alarm stage idle");
    }

    /// QA（#18 核心）：真实采样 → 引擎产生**真实告警记录** → `GET /api/alerts`
    /// 读到的就是这份仓库。阈值不满足时是**空列表**，绝不凭空造一条。
    #[tokio::test]
    async fn real_samples_produce_real_alarm_records() {
        let dir = tempfile::tempdir().expect("tempdir");
        let runtime = start_runtime(dir.path());
        // FakeInner 固定产出 12.5 → 阈值 `> 100` 不触发。
        let config = config_with_alarm(&alarm_rule_body("[p1] > 100"));
        let (live_tx, _live_rx) = broadcast::channel(8);
        let store = AlarmStore::shared();
        let plane = NorthDataPlane::new(
            Arc::new(FakeInner { n: 1 }),
            &config,
            Arc::new(ConfigShared::new(config.clone())),
            live_tx,
            test_health(),
            Arc::clone(&store),
        );
        plane.attach(Arc::clone(&runtime));
        let samples = plane
            .poll("dev-01", &["p1".to_string()])
            .await
            .expect("poll");
        assert_eq!(samples.len(), 1);
        assert_eq!(store.len(), 0, "value 12.5 must not trip `> 100`");
        assert_eq!(plane.stats().alarms_evaluated, 1);
        assert_eq!(plane.stats().alarms_fired, 0);

        // 阈值下调到 5 → 同一份样本必然触发（真实数据、真实记录）。
        let mut next = config.clone();
        next.alarms = Some(crate::config::AlarmsSection {
            enabled: true,
            rules: vec![crate::config::AlarmRuleConfig {
                id: "a1".to_string(),
                name: Some("超温告警".to_string()),
                enabled: true,
                condition: Some("[p1] > 5".to_string()),
                ..Default::default()
            }],
        });
        let shared = Arc::new(ConfigShared::new(next.clone()));
        let plane = NorthDataPlane::new(
            Arc::new(FakeInner { n: 1 }),
            &next,
            Arc::clone(&shared),
            broadcast::channel(8).0,
            test_health(),
            Arc::clone(&store),
        );
        plane.attach(Arc::clone(&runtime));
        plane
            .poll("dev-01", &["p1".to_string()])
            .await
            .expect("poll after the threshold change");

        let rows = store.snapshot();
        assert_eq!(rows.len(), 1, "one real record produced by the engine");
        assert_eq!(rows[0].rule_id, "a1");
        assert_eq!(rows[0].state, "open");
        assert_eq!(rows[0].count, 1);
        assert!(
            rows[0].first_seen_at.len() == 13,
            "毫秒 epoch 字符串（前端 formatEpochText 只认 10/13 位）"
        );
        assert_eq!(plane.stats().alarms_fired, 1);
    }

    /// QA（#18）：**北向运行期未挂载也要产出真实告警记录**——告警的出口是管理面
    /// 列表而不是北向 topic，没有出口（或启动竞态窗口内）的网关照样要记告警。
    #[tokio::test]
    async fn alarm_records_are_produced_without_the_north_runtime() {
        let dir = tempfile::tempdir().expect("tempdir");
        // 只建运行期句柄，**不** plane.attach（无出口 / 未就绪都走这条路径）。
        let _runtime = start_runtime(dir.path());
        let config = config_with_alarm(&alarm_rule_body("[p1] > 0"));
        let (live_tx, _live_rx) = broadcast::channel(8);
        let store = AlarmStore::shared();
        let plane = NorthDataPlane::new(
            Arc::new(FakeInner { n: 1 }),
            &config,
            Arc::new(ConfigShared::new(config.clone())),
            live_tx,
            test_health(),
            Arc::clone(&store),
        );
        assert!(!plane.is_attached(), "north runtime is not mounted here");

        let samples = plane
            .poll("dev-01", &["p1".to_string()])
            .await
            .expect("poll");
        assert_eq!(samples.len(), 1);

        let rows = store.snapshot();
        assert_eq!(
            rows.len(),
            1,
            "alarm must be recorded without a north runtime"
        );
        assert_eq!(rows[0].state, "open");
        assert_eq!(plane.stats().alarms_evaluated, 1);
        assert_eq!(plane.stats().admitted, 0, "still nothing delivered north");
    }

    /// QA（#18）：配置版本变化 → 告警引擎热重建（页面改规则下一拍即生效）。
    #[test]
    fn alarm_engine_is_rebuilt_when_the_config_version_moves() {
        let config = config_with_alarm(&alarm_rule_body("[p1] > 240"));
        let shared = Arc::new(ConfigShared::new(config.clone()));
        let plane = NorthDataPlane::new(
            Arc::new(FakeInner { n: 1 }),
            &config,
            Arc::clone(&shared),
            broadcast::channel(8).0,
            test_health(),
            AlarmStore::shared(),
        );
        assert!(plane.has_alarms());

        let version = shared.replace(config_with("mqtt://127.0.0.1:1883", "protobuf"));
        assert!(version > 1);
        plane.sync_alarm_engine();
        assert!(
            !plane.has_alarms(),
            "engine must be rebuilt after the version bump (no [alarms] in the new snapshot)"
        );
    }
}
