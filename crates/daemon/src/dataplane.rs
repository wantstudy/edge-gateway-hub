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

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard, OnceLock, PoisonError};

use async_trait::async_trait;
use base64::Engine as _;
use protocol_proto::TelemetryBatch;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::broadcast;
use tracing::{debug, error, warn};

use crate::backpressure::{AuditLog, PushOutcome};
use crate::config::GatewayConfig;
use crate::error::DaemonResult;
use crate::north::encoder::{encoder_for, sample_to_data_point, BatchEncoder, JsonEncoder};
use crate::north::runtime::NorthRuntime;
use crate::offline_queue::{Clock, SystemClock};
use crate::pipeline::{
    AcquisitionPipeline, DataProcessor, PointConfig as PipelinePointConfig, ProcessedSample,
    RawSample,
};
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
    /// `live_tx` 为管理面实时遥测广播发送端（[`DaemonShared::live_sender`] 注入）。
    #[must_use]
    pub fn new(
        inner: Arc<dyn PollHandler>,
        config: &GatewayConfig,
        live_tx: broadcast::Sender<LiveTelemetry>,
    ) -> Self {
        let gateway_id = config.gateway.gateway_id.clone();
        let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());

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
                    "dataplane: [ERROR] point pipeline build failed; northbound forwarding \
                     stays disabled (capture and scheduling continue; fix [[points]] to recover)"
                );
                None
            }
        };

        let lanes = config
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
        }
    }

    /// 挂载北向运行期句柄（晚绑定；幂等——重复 attach 忽略后者）。
    ///
    /// 北向启动期被拒绝（配置非法 / TLS 校验失败）的出口不产生泳道剔除之外的
    /// 任何处理：其 `error!` 已由 `NorthRuntime::start` 记录，这里仅摘除对应
    /// 泳道，避免每拍对未知出口反复报错。
    pub fn attach(&self, runtime: Arc<NorthRuntime>) {
        let started: HashSet<&str> = runtime.outlet_names().into_iter().collect();
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
        let Some(pipeline) = &self.pipeline else {
            return; // 管线配置非法：只采不转（构造期已 error!，不逐拍刷屏）。
        };
        let Some(runtime) = self.runtime.get() else {
            DataPlaneCounters::bump(&self.counters.pre_runtime_cycles);
            return; // 北向运行期未就绪（启动竞态窗口）：样本照常产生、本轮不投递。
        };
        DataPlaneCounters::bump(&self.counters.forward_cycles);

        let ts = self.clock.now_ns();
        let mut points = Vec::with_capacity(samples.len());
        {
            let mut pipeline = lock_or_recover(pipeline);
            for sample in samples {
                match pipeline.ingest_physical(sample.clone(), ts) {
                    Ok(Some(processed)) => {
                        DataPlaneCounters::bump(&self.counters.physical_emitted);
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
                points.push(sample_to_data_point(&ProcessedSample {
                    // 派生点无物理设备归属：以网关为 `device_id`（北向 schema 必填）。
                    device_id: self.gateway_id.clone(),
                    point_id: derived.point_id,
                    value,
                    unit: String::new(),
                    device_ts_ns: None,
                    collected_ts_ns: ts,
                    quality: derived.quality.to_wire(),
                }));
            }
        }
        if points.is_empty() {
            return; // 全部被死区过滤 / 求值失败：本拍无北向批次。
        }
        let batch = TelemetryBatch {
            points,
            ts,
            gateway_id: self.gateway_id.clone(),
            auth: None, // 签名块由既有北向签名链路负责，数据面不重复实现。
        };

        // 实时遥测扇出（task 52）：把解码后逐点遥测广播给管理面 `/api/stream`
        // 订阅者。复用 `JsonEncoder` 字段名、把 `value` 解码为 JSON 数值；
        // 仅 `Err`/`None` 丢弃、不 panic（broadcast 无订阅者 / 编码异常均为正常路径，
        // 不影响北向投递）。
        if let Some(live) = LiveTelemetry::from_batch(&batch) {
            let _ = self.live_tx.send(live);
        }

        let lanes = lock_or_recover(&self.lanes);
        for lane in lanes.iter() {
            let payload = match lane.encoder.encode_batch(&batch) {
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
        let samples = self.inner.poll(group, point_ids).await?;
        self.forward(&samples);
        Ok(samples)
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
        let plane = NorthDataPlane::new(Arc::new(FakeInner { n: 1 }), &config, live_tx);
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
        let plane = NorthDataPlane::new(Arc::new(FakeInner { n: 1 }), &config, live_tx);

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
        let plane = NorthDataPlane::new(Arc::new(FakeInner { n: 1 }), &config, live_tx);
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
            live_tx,
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
            live_tx,
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
}
