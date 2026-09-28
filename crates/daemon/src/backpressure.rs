//! 补发幂等去重 + 背压与内存队列水位（plan task 54，Wave 7）。
//!
//! ## 职责边界
//! - **做**：补发（replay）的幂等键判定与 high-water mark 语义；内存队列水位/硬上限与溢出策略；
//!   慢消费者保护（发送队列水位 + 落盘降级）；采集侧背压（持续高位 → 降采样 + 告警）；
//!   容量估算的可测试算式；所有丢弃/降级/拒绝的**审计留痕**。
//! - **不做**：队列本体（复用 [`crate::offline_queue::OfflineQueue`]，task 17 已落地，
//!   本模块不再另造一套队列）；MQTT 协议发送（task 19 域）；磁盘加密（task 18）。
//!
//! ## 关键设计决策
//!
//! 1. **幂等键 = `gateway_id + batch_seq`**（复用 [`crate::offline_queue::idempotency_key`]，
//!    格式 `{gateway_id}:{batch_seq}`）。补发 / 重放全过程保持稳定：同一条目被 `take_batch`
//!    取走两次，两次的幂等键相同，接收端按键去重即可保证断网重放不产生重复入库。
//! 2. **Ack 语义：先落 Ack，后推位点**（[`AckCursor::persist_then_advance`] /
//!    [`ReplayController::ack`]）。第一步把确认记录交给 [`AckSink`] 持久化（默认落
//!    `queue_meta.ack_seq`，可注入）；**持久化失败则位点一步不动**，数据继续保留，
//!    后续重放由幂等键去重。第二步（持久化成功后）才推进内存位点并裁剪去重窗口。
//!    这条顺序保证「位点推进」永远不先于「Ack 落盘」，从而不丢数据。
//! 3. **服务端（Broker）侧去重的责任边界 —— 明确说明**：
//!    - 客户自建 Broker（EMQX / Mosquitto 等）场景下，**MQTT Broker 只保证投递语义
//!      （QoS1 = 至少一次），不保证消费端不重复入库**；Broker 侧既无幂等键概念，
//!      也无法代业务方判定「这条数据是否已经入库」。因此**去重责任在消费端**。
//!    - 本模块给出的**参考实现** = [`IdempotencyLedger`]：消费端在入库前调用
//!      [`IdempotencyLedger::apply_key`]，返回 [`DedupOutcome::Applied`] 才入库，
//!      返回 [`DedupOutcome::Duplicate`] 则跳过（这是计划允许的唯一「隐式丢弃」：
//!      **忽略重复**）。参考接线：
//!      ```text
//!      for record in consumer.recv() {
//!          let key = record.idempotency_key;         // "gw-1:42"，来自 batch_meta_json
//!          if ledger.apply_key(&key).is_applied() {  // 去重判定
//!              sink.store(record)?;                  // 只有新键才入库
//!          }
//!          // 位点：入库成功后（且 Ack 已落盘）才 ledger.confirm_up_to(seq)
//!      }
//!      ```
//!    - 若接入的是**厂商云**（服务端可由我们控制），则服务端应按同一幂等键做唯一索引
//!      （`UNIQUE(gateway_id, batch_seq)`），重复插入返回冲突即丢弃；本模块不实现该
//!      服务端（属 licensing-server / 云端域），但键格式与客户端完全一致。
//!    - 跨进程 JSON 传输时 `batch_seq` 一律**字符串**（IEEE754 双精度上限 2^53−1），
//!      见 [`crate::offline_queue::batch_meta_json`] 与 [`audit_json`]。
//! 4. **水位三级 + 溢出策略**（与 [`crate::offline_queue::QueueConfig::high_water_rows`]
//!    完全对齐：`high_water = min(配置值, 硬上限 × 4/5)`，见 [`clamp_high_water`]）：
//!    - `Normal`：入内存；
//!    - `High`：降级落盘（不进内存），落盘不可用时才退回内存（仍受硬上限保护）；
//!    - `Critical`（硬上限）：拒绝入队，**数据交还调用方**并记
//!      [`BackpressureAudit::OverflowRejected`] —— **绝不静默、绝不阻塞采集线程**。
//! 5. **慢消费者保护**（[`SendQueue`]）：水位 = 已提交未确认（PUBACK/PUBCOMP）数。
//!    达水位即走注入的 [`SpillSink`] 落盘降级（生产实现 [`QueueSpillSink`] 落到
//!    `OfflineQueue`），**永不阻塞采集路径**（一次整数比较 + 一次有界落盘调用）；
//!    硬上限且落盘也失败时把数据**交还调用方 + 审计**，不在本模块丢弃。
//! 6. **采集侧背压**（[`AcquisitionGovernor`]）：队列**持续**高位（连续 `sustain_samples`
//!    次采样 ≥ 高水位）才按 `reduce_step_permille` 逐级降采样（放大轮询周期），
//!    带**迟滞**恢复（连续 `recover_samples` 次 ≤ 低水位才逐级恢复），避免抖动；
//!    已降到 `min_factor_permille` 仍高位 → [`BackpressureAudit::AcquisitionAlarm`] 告警。
//! 7. **容量估算**（[`CapacityEstimate`]）：全部走**整数算式**（无浮点、无 panic），
//!    口径与 `docs/design/capacity-estimation.md` 一致。
//!    ⚠ 算式红线：`速率 = 设备数 × 1000 / 采集周期ms`；50 设备 × 100ms = **500 条/秒**，
//!    断网 1h = **1,800,000 条**；200 设备 × 100ms = **2000 条/秒**，断网 1h = **7,200,000 条**。
//! 8. **零 panic**：非测试代码无 `unwrap` / `expect` / `panic!` / 除零 / 越界索引；
//!    全部用 `checked_*` / `saturating_*` 与守卫分支；锁中毒取回内部数据。
//!
//! ## 接线钩子清单（本模块不自行改他人文件）
//! - 采集入队路径：把 `queue.enqueue(payload)` 换成
//!   [`enqueue_with_backpressure`]（自动水位判定 + 溢出审计 + 数据交还）。
//! - 发送（MQTT）路径：构造 [`SendQueue`]（`spill` 传 [`QueueSpillSink`]），
//!   `push` → `take_ready` → 发布 → PUBACK 后 `confirm(n)`。
//! - 补发路径：构造 [`ReplayController`]（`sink` 传 `Arc<OfflineQueue>` 适配的
//!   [`QueueAckSink`]，或复用队列内置的 `ack_up_to`），`should_send` → 发送 → `ack`。
//! - 组轮询调度：每轮把 `AcquisitionGovernor::observe(rows).poll_interval_ms` 作为下一轮周期。

use std::collections::{BTreeSet, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::error::{DaemonError, DaemonResult};
use crate::offline_queue::{
    AckSink, Clock, OfflineQueue, QueueConfig, QueuedBatch, DEFAULT_MAX_DB_BYTES,
    DEFAULT_MAX_MEM_ROWS, DEFAULT_MEM_HIGH_WATER_ROWS,
};

// ==================== 1. 容量估算（可测试算式） ====================

/// 单条样本负载估算（字节）：protobuf 单点位 + 批量信封与页开销余量，
/// 与 `docs/design/capacity-estimation.md` §2 一致。
pub const DEFAULT_SAMPLE_BYTES: u64 = 300;

/// WAL 物理放大系数（千分比，1500 = ×1.5；见设计文档 §2「WAL 放大 ×1.5」）。
pub const DEFAULT_WAL_OVERHEAD_PERMILLE: u32 = 1500;

/// 7 天保留期（秒）。
pub const SEVEN_DAYS_SECONDS: u64 = 604_800;

/// 恢复期补发速率（条/秒，设计文档 §5 口径：2000 条/秒）。
pub const DEFAULT_REPLAY_RATE_PER_SEC: u64 = 2_000;

/// 千分比基数。
const PERMILLE: u128 = 1_000;

/// 容量估算输入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapacityInput {
    /// 设备数。
    pub device_count: u64,
    /// 单设备采集周期（毫秒）。
    pub poll_interval_ms: u64,
    /// 单条样本负载（字节）。
    pub sample_bytes: u64,
    /// 每批合并样本数（1 = 不合批）。
    pub samples_per_batch: u64,
    /// 磁盘队列上限（字节）。
    pub disk_cap_bytes: u64,
    /// 保留期（秒）。
    pub retention_seconds: u64,
    /// WAL 物理放大（千分比）。
    pub wal_overhead_permille: u32,
    /// 恢复期补发速率（条/秒）。
    pub replay_rate_per_sec: u64,
}

impl CapacityInput {
    /// 参考场景：**200 设备 × 100ms**（不合批，300 B/条，10 GiB 磁盘，7 天保留）。
    #[must_use]
    pub fn reference_200_devices() -> Self {
        Self {
            device_count: 200,
            poll_interval_ms: 100,
            sample_bytes: DEFAULT_SAMPLE_BYTES,
            samples_per_batch: 1,
            disk_cap_bytes: DEFAULT_MAX_DB_BYTES,
            retention_seconds: SEVEN_DAYS_SECONDS,
            wal_overhead_permille: DEFAULT_WAL_OVERHEAD_PERMILLE,
            replay_rate_per_sec: DEFAULT_REPLAY_RATE_PER_SEC,
        }
    }

    /// 参考场景：**50 设备 × 100ms**（计划算式红线场景）。
    #[must_use]
    pub fn reference_50_devices() -> Self {
        Self {
            device_count: 50,
            ..Self::reference_200_devices()
        }
    }

    /// 参考场景：200 设备 × 100ms，**4 样本合 1 批**上传（设计文档 §1 第三行）。
    #[must_use]
    pub fn reference_200_devices_batched() -> Self {
        Self {
            samples_per_batch: 4,
            ..Self::reference_200_devices()
        }
    }
}

/// 容量瓶颈：决定淘汰由谁主导。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapacityBottleneck {
    /// 保留期窗口内的数据量**装得下**磁盘上限 → 淘汰由 `retention_ns` 主导。
    RetentionWindow,
    /// 保留期窗口内的数据量**超出**磁盘上限 → 淘汰由 `max_db_bytes` 环形覆盖主导。
    DiskCapacity,
}

/// 容量估算结果（全部由整数算式得出，可断言）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapacityEstimate {
    /// 采集速率（样本条/秒）。
    pub samples_per_sec: u64,
    /// 入队速率（批/秒；合批后）。
    pub batches_per_sec: u64,
    /// 断网 1 小时累积条数（批次口径 = 合批后）。
    pub rows_per_hour: u64,
    /// 逻辑字节/秒（未计 WAL 放大）。
    pub logical_bytes_per_sec: u64,
    /// 物理字节/秒（计 WAL 放大）。
    pub physical_bytes_per_sec: u64,
    /// 保留期窗口内累积条数（7 天默认 = 604800 s）。
    pub rows_in_retention: u128,
    /// 保留期窗口内累积字节数。
    pub bytes_in_retention: u128,
    /// 磁盘上限可支撑的秒数（逻辑口径）。
    pub disk_covered_seconds_logical: u64,
    /// 磁盘上限可支撑的秒数（物理口径，计 WAL 放大）。
    pub disk_covered_seconds_physical: u64,
    /// 磁盘上限可容纳的条数（物理口径）。
    pub disk_capacity_rows: u64,
    /// 保留期窗口的数据量是否能装进磁盘上限。
    pub fits_retention_window: bool,
    /// 实际主导淘汰的因素。
    pub bottleneck: CapacityBottleneck,
    /// 断网 1 小时后按 `replay_rate_per_sec` 追平所需秒数。
    pub replay_seconds_one_hour_outage: u64,
}

/// 采集速率（条/秒）= `设备数 × 1000 / 采集周期ms`（整数下取整）。
///
/// `poll_interval_ms == 0` 或 `device_count == 0` 时返回 0（**绝不除零**）。
#[must_use]
pub fn samples_per_second(device_count: u64, poll_interval_ms: u64) -> u64 {
    if device_count == 0 || poll_interval_ms == 0 {
        return 0;
    }
    let numerator = u128::from(device_count).saturating_mul(1_000);
    let rate = match numerator.checked_div(u128::from(poll_interval_ms)) {
        Some(v) => v,
        None => return 0,
    };
    u64::try_from(rate).unwrap_or(u64::MAX)
}

/// 时间窗口内累积条数 = `设备数 × 1000 × 窗口秒 / 采集周期ms`（`u128` 中间量，不溢出）。
#[must_use]
pub fn rows_in_window(device_count: u64, poll_interval_ms: u64, window_seconds: u64) -> u128 {
    if device_count == 0 || poll_interval_ms == 0 || window_seconds == 0 {
        return 0;
    }
    let numerator = u128::from(device_count)
        .saturating_mul(1_000)
        .saturating_mul(u128::from(window_seconds));
    numerator
        .checked_div(u128::from(poll_interval_ms))
        .unwrap_or(0)
}

/// 条数 → 字节数（`u128` 饱和，绝不溢出）。
#[must_use]
pub fn bytes_for_rows(rows: u128, sample_bytes: u64) -> u128 {
    rows.saturating_mul(u128::from(sample_bytes))
}

/// 磁盘上限可支撑秒数 = `cap / bytes_per_sec`；速率为 0 时视为「无限」（`u64::MAX`）。
#[must_use]
pub fn disk_covered_seconds(bytes_per_sec: u64, disk_cap_bytes: u64) -> u64 {
    if bytes_per_sec == 0 {
        return u64::MAX;
    }
    u128::from(disk_cap_bytes)
        .checked_div(u128::from(bytes_per_sec))
        .and_then(|v| u64::try_from(v).ok())
        .unwrap_or(u64::MAX)
}

/// 补发追平耗时（秒）= `rows / rate`；速率为 0 时返回 `u64::MAX`（永远追不平）。
#[must_use]
pub fn replay_seconds(rows: u64, rate_per_sec: u64) -> u64 {
    if rate_per_sec == 0 {
        return u64::MAX;
    }
    rows / rate_per_sec
}

/// 按千分比放大（用于 WAL 系数）：`value × permille / 1000`（饱和，不溢出）。
#[must_use]
fn apply_permille(value: u64, permille: u32) -> u64 {
    let scaled = u128::from(value).saturating_mul(u128::from(permille));
    let out = scaled.checked_div(PERMILLE).unwrap_or(0);
    u64::try_from(out).unwrap_or(u64::MAX)
}

/// 完整容量估算（纯函数，可断言；见 [`CapacityEstimate`]）。
#[must_use]
pub fn estimate(input: &CapacityInput) -> CapacityEstimate {
    let samples_per_sec = samples_per_second(input.device_count, input.poll_interval_ms);
    let batches_per_sec = if input.samples_per_batch <= 1 {
        samples_per_sec
    } else {
        samples_per_sec / input.samples_per_batch
    };

    // 条数口径统一取「批次」：合批后入队条数 = 样本数 / 每批样本数。
    let rows_per_hour_u128 = match input.samples_per_batch {
        0 | 1 => rows_in_window(input.device_count, input.poll_interval_ms, 3_600),
        n => rows_in_window(input.device_count, input.poll_interval_ms, 3_600)
            .checked_div(u128::from(n))
            .unwrap_or(0),
    };
    let rows_in_retention_u128 = match input.samples_per_batch {
        0 | 1 => rows_in_window(
            input.device_count,
            input.poll_interval_ms,
            input.retention_seconds,
        ),
        n => rows_in_window(
            input.device_count,
            input.poll_interval_ms,
            input.retention_seconds,
        )
        .checked_div(u128::from(n))
        .unwrap_or(0),
    };

    // 字节口径按**样本**计（合批不省字节，只省行开销）。
    let sample_rows_per_hour = rows_in_window(input.device_count, input.poll_interval_ms, 3_600);
    let sample_rows_retention = rows_in_window(
        input.device_count,
        input.poll_interval_ms,
        input.retention_seconds,
    );
    let bytes_in_retention = bytes_for_rows(sample_rows_retention, input.sample_bytes);

    let logical_bytes_per_sec =
        u128::from(samples_per_sec).saturating_mul(u128::from(input.sample_bytes));
    let logical_bytes_per_sec = u64::try_from(logical_bytes_per_sec).unwrap_or(u64::MAX);
    let physical_bytes_per_sec = apply_permille(logical_bytes_per_sec, input.wal_overhead_permille);

    let physical_row_bytes = apply_permille(input.sample_bytes, input.wal_overhead_permille).max(1);
    let disk_capacity_rows = u128::from(input.disk_cap_bytes)
        .checked_div(u128::from(physical_row_bytes))
        .and_then(|v| u64::try_from(v).ok())
        .unwrap_or(u64::MAX);

    let rows_per_hour = u64::try_from(rows_per_hour_u128).unwrap_or(u64::MAX);
    let fits_retention_window = bytes_in_retention <= u128::from(input.disk_cap_bytes);

    CapacityEstimate {
        samples_per_sec,
        batches_per_sec,
        rows_per_hour,
        logical_bytes_per_sec,
        physical_bytes_per_sec,
        rows_in_retention: rows_in_retention_u128,
        bytes_in_retention,
        disk_covered_seconds_logical: disk_covered_seconds(
            logical_bytes_per_sec,
            input.disk_cap_bytes,
        ),
        disk_covered_seconds_physical: disk_covered_seconds(
            physical_bytes_per_sec,
            input.disk_cap_bytes,
        ),
        disk_capacity_rows,
        fits_retention_window,
        bottleneck: if fits_retention_window {
            CapacityBottleneck::RetentionWindow
        } else {
            CapacityBottleneck::DiskCapacity
        },
        replay_seconds_one_hour_outage: replay_seconds(
            u64::try_from(sample_rows_per_hour).unwrap_or(u64::MAX),
            input.replay_rate_per_sec,
        ),
    }
}

// ==================== 2. 幂等去重与位点（Ack 语义） ====================

/// 解析幂等键 `{gateway_id}:{batch_seq}` → `(gateway_id, batch_seq)`。
///
/// 无 `:` 或序号不可解析时返回 `None`（**绝不 panic**）。
#[must_use]
pub fn parse_idempotency_key(key: &str) -> Option<(&str, u64)> {
    let (gateway_id, seq_text) = key.rsplit_once(':')?;
    if gateway_id.is_empty() || seq_text.is_empty() {
        return None;
    }
    let seq = seq_text.parse::<u64>().ok()?;
    Some((gateway_id, seq))
}

/// 取幂等键中的 `batch_seq`。
#[must_use]
pub fn batch_seq_of_key(key: &str) -> Option<u64> {
    parse_idempotency_key(key).map(|(_, seq)| seq)
}

/// 重复原因（去重判定的可诊断依据）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DedupReason {
    /// `seq <= high_water`：该批已被确认（位点已推进），属重放。
    BelowHighWater {
        /// 当前 high-water mark。
        high_water: u64,
    },
    /// 窗口内已应用过（在途重放 / 网络重复投递）。
    AlreadyApplied,
    /// 键属于另一个网关（单网关登记簿不处理，由调用方分派）。
    OtherGateway {
        /// 登记簿绑定的网关标识。
        expected: String,
        /// 键中的网关标识。
        found: String,
    },
    /// 键格式非法（无法判定 → **不入库**，交由调用方告警，绝不静默通过）。
    MalformedKey {
        /// 原始键。
        key: String,
    },
}

/// 去重判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DedupOutcome {
    /// 新数据：可以入库 / 可以发送。
    Applied,
    /// 重复：跳过（计划允许的唯一隐式丢弃 = 忽略重复）。
    Duplicate {
        /// 判定为重复的原因。
        reason: DedupReason,
    },
}

impl DedupOutcome {
    /// 是否可以入库 / 发送。
    #[must_use]
    pub fn is_applied(&self) -> bool {
        matches!(self, Self::Applied)
    }

    /// 是否应跳过。
    #[must_use]
    pub fn is_duplicate(&self) -> bool {
        matches!(self, Self::Duplicate { .. })
    }
}

/// 去重登记簿统计（可观测）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LedgerStats {
    /// 已应用（入库）条数。
    pub applied_rows: u64,
    /// 被判定为重复而跳过的条数。
    pub duplicate_rows: u64,
    /// 当前窗口内跟踪的 seq 数。
    pub tracked: usize,
    /// 当前 high-water mark。
    pub high_water: u64,
}

/// 消费端去重登记簿（**客户自建 Broker 场景的参考实现**）。
///
/// 语义（计划允许的唯一隐式丢弃 = 忽略重复）：
/// - `seq <= high_water` → 已确认过的重放，跳过；
/// - 窗口内已出现过 → 重复投递，跳过；
/// - 否则视为新数据 → 记录并返回 [`DedupOutcome::Applied`]。
///
/// **有界内存**：`max_tracked` 限制窗口内跟踪的 seq 数（超出后淘汰最小的 seq）。
/// 窗口外（极老的未确认 seq）无法判定重复 —— 这正是「先落 Ack 后推位点」的意义：
/// 位点推进后窗口内条目被裁剪，正常路径不会碰到窗口外判定。
pub struct IdempotencyLedger {
    gateway_id: String,
    applied: BTreeSet<u64>,
    high_water: u64,
    max_tracked: usize,
    applied_rows: u64,
    duplicate_rows: u64,
}

impl IdempotencyLedger {
    /// 构造（`gateway_id` 相同的键才由本登记簿处理；`max_tracked` 至少 1）。
    #[must_use]
    pub fn new(gateway_id: impl Into<String>, max_tracked: usize) -> Self {
        Self {
            gateway_id: gateway_id.into(),
            applied: BTreeSet::new(),
            high_water: 0,
            max_tracked: max_tracked.max(1),
            applied_rows: 0,
            duplicate_rows: 0,
        }
    }

    /// 当前 high-water mark（已确认到的最大 `batch_seq`）。
    #[must_use]
    pub fn high_water(&self) -> u64 {
        self.high_water
    }

    /// 绑定网关标识。
    #[must_use]
    pub fn gateway_id(&self) -> &str {
        &self.gateway_id
    }

    /// 统计。
    #[must_use]
    pub fn stats(&self) -> LedgerStats {
        LedgerStats {
            applied_rows: self.applied_rows,
            duplicate_rows: self.duplicate_rows,
            tracked: self.applied.len(),
            high_water: self.high_water,
        }
    }

    /// 按 `batch_seq` 判定去重。
    pub fn apply_seq(&mut self, seq: u64) -> DedupOutcome {
        if seq <= self.high_water {
            self.duplicate_rows = self.duplicate_rows.saturating_add(1);
            return DedupOutcome::Duplicate {
                reason: DedupReason::BelowHighWater {
                    high_water: self.high_water,
                },
            };
        }
        if !self.applied.insert(seq) {
            self.duplicate_rows = self.duplicate_rows.saturating_add(1);
            return DedupOutcome::Duplicate {
                reason: DedupReason::AlreadyApplied,
            };
        }
        self.applied_rows = self.applied_rows.saturating_add(1);
        self.trim_window();
        DedupOutcome::Applied
    }

    /// 按幂等键 `{gateway_id}:{batch_seq}` 判定去重（**消费端入口**）。
    ///
    /// 键属于其它网关 → [`DedupReason::OtherGateway`]；键格式非法 →
    /// [`DedupReason::MalformedKey`]（不入库，交由调用方告警）。
    pub fn apply_key(&mut self, key: &str) -> DedupOutcome {
        match parse_idempotency_key(key) {
            None => {
                self.duplicate_rows = self.duplicate_rows.saturating_add(1);
                DedupOutcome::Duplicate {
                    reason: DedupReason::MalformedKey {
                        key: key.to_string(),
                    },
                }
            }
            Some((gateway_id, _)) if gateway_id != self.gateway_id => {
                self.duplicate_rows = self.duplicate_rows.saturating_add(1);
                DedupOutcome::Duplicate {
                    reason: DedupReason::OtherGateway {
                        expected: self.gateway_id.clone(),
                        found: gateway_id.to_string(),
                    },
                }
            }
            Some((_, seq)) => self.apply_seq(seq),
        }
    }

    /// 按 [`QueuedBatch`] 判定去重（复用其 [`QueuedBatch::idempotency_key`]）。
    pub fn apply_batch(&mut self, batch: &QueuedBatch) -> DedupOutcome {
        self.apply_key(&batch.idempotency_key())
    }

    /// 位点推进后裁剪窗口（`seq <= high_water` 的条目不再需要跟踪）。
    ///
    /// 由 [`ReplayController::ack`] 在「Ack 落盘成功」后调用；**不得**在落盘前调用。
    pub fn confirm_up_to(&mut self, seq: u64) {
        if seq <= self.high_water {
            return;
        }
        self.high_water = seq;
        // 保留 seq > high_water 的条目。
        self.applied = self.applied.split_off(&seq.saturating_add(1));
    }

    /// 窗口裁剪（有界内存）：淘汰最小的 seq，直到不超过 `max_tracked`。
    fn trim_window(&mut self) {
        while self.applied.len() > self.max_tracked {
            let Some(min) = self.applied.iter().next().copied() else {
                return;
            };
            self.applied.remove(&min);
        }
    }
}

/// Ack 位点游标：**先落 Ack，后推位点**。
///
/// 复用 task 17 的注入点 [`AckSink`]（`persist_ack` 返回 `Ok` 即表示确认记录已持久化）。
pub struct AckCursor {
    sink: Arc<dyn AckSink>,
    cursor: u64,
}

impl AckCursor {
    /// 构造（`start_seq` = 重启后读回的 high-water mark，通常取 `queue.ack_seq()`）。
    #[must_use]
    pub fn new(sink: Arc<dyn AckSink>, start_seq: u64) -> Self {
        Self {
            sink,
            cursor: start_seq,
        }
    }

    /// 当前 high-water mark。
    #[must_use]
    pub fn high_water_mark(&self) -> u64 {
        self.cursor
    }

    /// **先落 Ack，后推位点**：
    /// 1. [`AckSink::persist_ack`] 成功 → 位点推进到 `seq` 并返回 `Ok(())`；
    /// 2. 失败 → 返回 `Err`，**位点一步不动**（数据继续保留，重放由幂等键去重）。
    ///
    /// # Errors
    /// Ack 持久化失败（`StorageError` 4000 等）时返回错误，位点保持不变。
    pub fn persist_then_advance(&mut self, seq: u64) -> DaemonResult<()> {
        if seq <= self.cursor {
            return Ok(());
        }
        self.sink.persist_ack(seq)?;
        self.cursor = seq;
        Ok(())
    }
}

impl fmt::Debug for AckCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AckCursor")
            .field("cursor", &self.cursor)
            .finish_non_exhaustive()
    }
}

/// Ack 推进结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckOutcome {
    /// 位点已推进（Ack 已落盘）。
    Advanced {
        /// 推进前位点。
        from: u64,
        /// 推进后位点。
        to: u64,
    },
    /// `seq <= 当前位点`：无需推进（幂等）。
    AlreadyAcked {
        /// 当前位点。
        cursor: u64,
    },
}

/// 补发控制器：去重判定 + 位点推进（**先落 Ack 后推位点**）。
pub struct ReplayController {
    ledger: IdempotencyLedger,
    cursor: AckCursor,
}

impl ReplayController {
    /// 构造（`start_high_water` 通常取 `queue.ack_seq()`）。
    #[must_use]
    pub fn new(
        gateway_id: impl Into<String>,
        sink: Arc<dyn AckSink>,
        start_high_water: u64,
    ) -> Self {
        Self {
            ledger: IdempotencyLedger::new(gateway_id, DEFAULT_MEM_HIGH_WATER_ROWS),
            cursor: AckCursor::new(sink, start_high_water),
        }
    }

    /// 当前 high-water mark（已确认到的最大 `batch_seq`）。
    #[must_use]
    pub fn high_water_mark(&self) -> u64 {
        self.cursor.high_water_mark()
    }

    /// 该 `seq` 是否需要补发（`seq > high_water` 且窗口内未发过）。
    #[must_use]
    pub fn should_send(&self, seq: u64) -> bool {
        seq > self.cursor.high_water_mark() && !self.ledger.applied.contains(&seq)
    }

    /// 标记已发送（去重判定）：返回 [`DedupOutcome::Applied`] 才真正发送。
    pub fn mark_sent(&mut self, seq: u64) -> DedupOutcome {
        self.ledger.apply_seq(seq)
    }

    /// 按幂等键标记已发送（消费端 / 北向出口共用语义）。
    pub fn mark_sent_key(&mut self, key: &str) -> DedupOutcome {
        self.ledger.apply_key(key)
    }

    /// 确认：**先落 Ack（[`AckSink::persist_ack`]），成功后才推进位点并裁剪去重窗口**。
    ///
    /// # Errors
    /// Ack 持久化失败时返回错误，**位点与去重窗口均不动**（补发会重发同一批，
    /// 由幂等键在消费端去重，不产生重复入库）。
    pub fn ack(&mut self, seq: u64) -> DaemonResult<AckOutcome> {
        let from = self.cursor.high_water_mark();
        if seq <= from {
            return Ok(AckOutcome::AlreadyAcked { cursor: from });
        }
        self.cursor.persist_then_advance(seq)?;
        self.ledger.confirm_up_to(seq);
        Ok(AckOutcome::Advanced { from, to: seq })
    }

    /// 去重登记簿（只读）。
    #[must_use]
    pub fn ledger(&self) -> &IdempotencyLedger {
        &self.ledger
    }
}

impl fmt::Debug for ReplayController {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReplayController")
            .field("high_water", &self.high_water_mark())
            .field("stats", &self.ledger.stats())
            .finish_non_exhaustive()
    }
}

/// 补发选取：过滤 `seq > high_water`，按 `seq` 升序并**按幂等键去重**。
///
/// 与 [`OfflineQueue::replay_batch`] 配套使用：队列返回的条目先过此函数，
/// 再交给 [`ReplayController::mark_sent`] / 消费端 [`IdempotencyLedger`]。
#[must_use]
pub fn replay_selection<'a>(batches: &'a [QueuedBatch], high_water: u64) -> Vec<&'a QueuedBatch> {
    let mut selected: Vec<&'a QueuedBatch> =
        batches.iter().filter(|b| b.seq > high_water).collect();
    selected.sort_by_key(|b| b.seq);
    let mut seen: BTreeSet<String> = BTreeSet::new();
    selected.retain(|b| seen.insert(b.idempotency_key()));
    selected
}

/// 读取队列的 high-water mark（复用 [`OfflineQueue::ack_seq`]）。
#[must_use]
pub fn high_water_mark(queue: &OfflineQueue) -> u64 {
    queue.ack_seq()
}

/// `AckSink` 参考实现：把 Ack 落到 [`OfflineQueue`]（其内部同样是「先落后推」）。
pub struct QueueAckSink {
    queue: Arc<OfflineQueue>,
}

impl QueueAckSink {
    /// 构造。
    #[must_use]
    pub fn new(queue: Arc<OfflineQueue>) -> Self {
        Self { queue }
    }

    /// 底层队列。
    #[must_use]
    pub fn queue(&self) -> &Arc<OfflineQueue> {
        &self.queue
    }
}

impl AckSink for QueueAckSink {
    fn persist_ack(&self, seq: u64) -> DaemonResult<()> {
        self.queue.ack_up_to(seq)
    }
}

/// `AckSink` 内存实现（测试 / 未接队列场景的参考实现）。
pub struct InMemoryAckSink {
    persisted: Mutex<u64>,
    calls: AtomicU64,
    fail: AtomicBool,
}

impl InMemoryAckSink {
    /// 构造（`start_seq` = 起始位点）。
    #[must_use]
    pub fn new(start_seq: u64) -> Self {
        Self {
            persisted: Mutex::new(start_seq),
            calls: AtomicU64::new(0),
            fail: AtomicBool::new(false),
        }
    }

    /// 设置「持久化失败」开关（模拟 Ack 落盘失败）。
    pub fn set_fail(&self, fail: bool) {
        self.fail.store(fail, Ordering::SeqCst);
    }

    /// 已持久化的位点（失败时不更新）。
    #[must_use]
    pub fn persisted(&self) -> u64 {
        *lock(&self.persisted)
    }

    /// `persist_ack` 被调用次数。
    #[must_use]
    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::SeqCst)
    }
}

impl AckSink for InMemoryAckSink {
    fn persist_ack(&self, seq: u64) -> DaemonResult<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            return Err(DaemonError::StorageError(format!(
                "ack persistence failed for batch_seq {seq} (injected failure)"
            )));
        }
        let mut guard = lock(&self.persisted);
        if seq > *guard {
            *guard = seq;
        }
        Ok(())
    }
}

impl fmt::Debug for InMemoryAckSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InMemoryAckSink")
            .field("persisted", &self.persisted())
            .field("calls", &self.calls())
            .finish_non_exhaustive()
    }
}

// ==================== 3. 水位与溢出策略 ====================

/// 高水位分子（4/5 = 80%，与 `offline_queue` 内部夹紧规则一致）。
pub const HIGH_WATER_NUM: usize = 4;

/// 高水位分母。
pub const HIGH_WATER_DEN: usize = 5;

/// 高水位夹紧：`min(high_water.max(1), hard_limit × 4/5)`，保证「降级」先于「拒绝」。
#[must_use]
pub fn clamp_high_water(high_water: usize, hard_limit: usize) -> usize {
    let cap = (hard_limit.saturating_mul(HIGH_WATER_NUM) / HIGH_WATER_DEN).max(1);
    high_water.max(1).min(cap)
}

/// 队列水位快照（行数 + 字节数）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Gauge {
    /// 当前行数。
    pub rows: usize,
    /// 当前字节数（不可得时填 0，退化成纯行数判定）。
    pub bytes: usize,
}

impl Gauge {
    /// 构造。
    #[must_use]
    pub fn new(rows: usize, bytes: usize) -> Self {
        Self { rows, bytes }
    }
}

/// 水位等级。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaterLevel {
    /// 正常：入内存。
    Normal,
    /// 高水位：降级落盘。
    High,
    /// 硬上限：拒绝入队 + 审计。
    Critical,
}

/// 背压动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackpressureAction {
    /// 允许入内存。
    Admit,
    /// 降级：走落盘路径（不进内存）。
    Degrade {
        /// 降级原因（可诊断）。
        reason: String,
    },
    /// 拒绝：数据**交还调用方**（绝不静默丢），并记审计。
    Reject {
        /// 拒绝原因（可诊断）。
        reason: String,
    },
}

/// 水位配置（行数 + 字节数双维度）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaterMarkConfig {
    /// 高水位行数。
    pub high_water_rows: usize,
    /// 硬上限行数。
    pub hard_limit_rows: usize,
    /// 高水位字节数。
    pub high_water_bytes: usize,
    /// 硬上限字节数。
    pub hard_limit_bytes: usize,
}

impl WaterMarkConfig {
    /// 从 [`QueueConfig`] 派生（**复用 task 17 的夹紧规则**：`high_water_rows()` /
    /// `high_water_bytes()` 已确保 ≤ 硬上限的 4/5）。
    #[must_use]
    pub fn from_queue_config(cfg: &QueueConfig) -> Self {
        Self {
            high_water_rows: cfg.high_water_rows(),
            hard_limit_rows: cfg.max_mem_rows,
            high_water_bytes: cfg.high_water_bytes(),
            hard_limit_bytes: cfg.max_mem_bytes,
        }
    }

    /// 独立构造（发送队列等不走 `QueueConfig` 的场景），并按 4/5 规则夹紧。
    #[must_use]
    pub fn new(high_water_rows: usize, hard_limit_rows: usize) -> Self {
        Self {
            high_water_rows: clamp_high_water(high_water_rows, hard_limit_rows.max(1)),
            hard_limit_rows: hard_limit_rows.max(1),
            high_water_bytes: usize::MAX,
            hard_limit_bytes: usize::MAX,
        }
    }

    /// 重新夹紧（配置被外部改动后调用）。
    ///
    /// **保留禁用哨兵**：`usize::MAX` 表示该维度**不启用**（见 [`Self::new`]）。
    /// 若对其做 `× 4/5` 重缩放，会把「禁用」变成一个真实的超大阈值（≈ 3.68e18），
    /// 从而在 [`Self::level`] 中误激活字节维度判定。
    pub fn clamp(&mut self) {
        self.high_water_rows = clamp_high_water(self.high_water_rows, self.hard_limit_rows.max(1));
        if self.high_water_bytes != usize::MAX {
            self.high_water_bytes =
                clamp_high_water(self.high_water_bytes, self.hard_limit_bytes.max(1));
        }
    }

    /// 水位等级判定（行数 / 字节数任一触达即升级）。
    #[must_use]
    pub fn level(&self, gauge: Gauge) -> WaterLevel {
        if gauge.rows >= self.hard_limit_rows || gauge.bytes >= self.hard_limit_bytes {
            return WaterLevel::Critical;
        }
        if gauge.rows >= self.high_water_rows || gauge.bytes >= self.high_water_bytes {
            return WaterLevel::High;
        }
        WaterLevel::Normal
    }

    /// 由水位导出动作（**唯一的策略出口**）。
    #[must_use]
    pub fn action(&self, gauge: Gauge) -> BackpressureAction {
        match self.level(gauge) {
            WaterLevel::Normal => BackpressureAction::Admit,
            WaterLevel::High => BackpressureAction::Degrade {
                reason: format!(
                    "high water reached (rows {}/{}, bytes {}/{}): degrade to disk",
                    gauge.rows, self.high_water_rows, gauge.bytes, self.high_water_bytes
                ),
            },
            WaterLevel::Critical => BackpressureAction::Reject {
                reason: format!(
                    "hard limit reached (rows {}/{}, bytes {}/{}): enqueue rejected, \
                     data returned to caller (never silently dropped)",
                    gauge.rows, self.hard_limit_rows, gauge.bytes, self.hard_limit_bytes
                ),
            },
        }
    }
}

impl Default for WaterMarkConfig {
    /// 默认取 `offline_queue` 的默认水位（8192 / 65536，字节维度不启用）。
    fn default() -> Self {
        Self::new(DEFAULT_MEM_HIGH_WATER_ROWS, DEFAULT_MAX_MEM_ROWS)
    }
}

// ==================== 4. 审计（丢数据必须留痕） ====================

/// 审计环形缓冲默认容量。
pub const DEFAULT_AUDIT_CAPACITY: usize = 4_096;

/// 背压审计事件：**任何**降级 / 落盘 / 拒绝 / 降采样 / 告警都留痕。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackpressureAudit {
    /// 进入高水位（一次，退出前不重复）。
    HighWaterEntered {
        /// 当时行数。
        rows: u64,
        /// 当时字节数。
        bytes: u64,
        /// 高水位行数。
        high_water_rows: u64,
        /// 事件时间（纳秒）。
        ts_ns: i64,
    },
    /// 退出高水位（迟滞恢复）。
    HighWaterCleared {
        /// 当时行数。
        rows: u64,
        /// 当时字节数。
        bytes: u64,
        /// 事件时间（纳秒）。
        ts_ns: i64,
    },
    /// **拒绝入队**（硬上限）：数据已交还调用方，此处仅留痕。
    OverflowRejected {
        /// 被拒批次序号（`0` = 序号未分配/不可得）。
        seq: u64,
        /// 当时行数。
        rows: u64,
        /// 当时字节数。
        bytes: u64,
        /// 触达的上限。
        limit: u64,
        /// 原因（可诊断）。
        reason: String,
        /// 事件时间（纳秒）。
        ts_ns: i64,
    },
    /// 降级落盘成功。
    DegradedToDisk {
        /// 落盘条数。
        rows: u64,
        /// 落盘字节数。
        bytes: u64,
        /// 原因。
        reason: String,
        /// 事件时间（纳秒）。
        ts_ns: i64,
    },
    /// 慢消费者：发送队列超限，已落盘降级。
    SlowConsumerSpilled {
        /// 当前在途（已提交未确认）条数。
        pending: u64,
        /// 发送高水位。
        high_water: u64,
        /// 本次落盘条数。
        spilled: u64,
        /// 原因。
        reason: String,
        /// 事件时间（纳秒）。
        ts_ns: i64,
    },
    /// 落盘降级不可用 → 退回内存（内存仍受硬上限保护），**留痕说明降级未生效**。
    SpillFailedAdmitted {
        /// 当前在途条数（落盘前）。
        pending: u64,
        /// 原因。
        reason: String,
        /// 事件时间（纳秒）。
        ts_ns: i64,
    },
    /// 采集侧降采样（轮询周期放大）。
    SamplingReduced {
        /// 当时队列行数。
        rows: u64,
        /// 连续高位采样次数。
        dwell_samples: u32,
        /// 降采样后的频率因子（千分比，500 = 原频率的 1/2）。
        factor_permille: u32,
        /// 降采样后的轮询周期（毫秒）。
        poll_interval_ms: u64,
        /// 事件时间（纳秒）。
        ts_ns: i64,
    },
    /// 采集侧恢复采样频率（迟滞恢复）。
    SamplingRestored {
        /// 当时队列行数。
        rows: u64,
        /// 连续低位采样次数。
        dwell_samples: u32,
        /// 恢复后的频率因子（千分比，1000 = 原频率）。
        factor_permille: u32,
        /// 恢复后的轮询周期（毫秒）。
        poll_interval_ms: u64,
        /// 事件时间（纳秒）。
        ts_ns: i64,
    },
    /// 采集侧告警：已降到最低采样频率仍持续高位（需运维介入）。
    AcquisitionAlarm {
        /// 当时队列行数。
        rows: u64,
        /// 高水位行数。
        high_water_rows: u64,
        /// 当前频率因子（千分比）。
        factor_permille: u32,
        /// 原因。
        reason: String,
        /// 事件时间（纳秒）。
        ts_ns: i64,
    },
}

impl BackpressureAudit {
    /// 事件类型名（JSON `kind` 字段）。
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::HighWaterEntered { .. } => "HighWaterEntered",
            Self::HighWaterCleared { .. } => "HighWaterCleared",
            Self::OverflowRejected { .. } => "OverflowRejected",
            Self::DegradedToDisk { .. } => "DegradedToDisk",
            Self::SlowConsumerSpilled { .. } => "SlowConsumerSpilled",
            Self::SpillFailedAdmitted { .. } => "SpillFailedAdmitted",
            Self::SamplingReduced { .. } => "SamplingReduced",
            Self::SamplingRestored { .. } => "SamplingRestored",
            Self::AcquisitionAlarm { .. } => "AcquisitionAlarm",
        }
    }
}

/// 审计事件 → JSON（**大数红线**：`seq` / 纳秒时间戳 / 字节计数一律编码为字符串）。
#[must_use]
pub fn audit_json(audit: &BackpressureAudit) -> String {
    match audit {
        BackpressureAudit::HighWaterEntered {
            rows,
            bytes,
            high_water_rows,
            ts_ns,
        } => format!(
            "{{\"kind\":\"{}\",\"rows\":\"{rows}\",\"bytes\":\"{bytes}\",\
             \"high_water_rows\":\"{high_water_rows}\",\"ts_ns\":\"{ts_ns}\"}}",
            audit.kind()
        ),
        BackpressureAudit::HighWaterCleared { rows, bytes, ts_ns } => format!(
            "{{\"kind\":\"{}\",\"rows\":\"{rows}\",\"bytes\":\"{bytes}\",\"ts_ns\":\"{ts_ns}\"}}",
            audit.kind()
        ),
        BackpressureAudit::OverflowRejected {
            seq,
            rows,
            bytes,
            limit,
            reason,
            ts_ns,
        } => format!(
            "{{\"kind\":\"{}\",\"batch_seq\":\"{seq}\",\"rows\":\"{rows}\",\"bytes\":\"{bytes}\",\
             \"limit\":\"{limit}\",\"reason\":\"{}\",\"ts_ns\":\"{ts_ns}\"}}",
            audit.kind(),
            json_escape(reason)
        ),
        BackpressureAudit::DegradedToDisk {
            rows,
            bytes,
            reason,
            ts_ns,
        } => format!(
            "{{\"kind\":\"{}\",\"rows\":\"{rows}\",\"bytes\":\"{bytes}\",\"reason\":\"{}\",\
             \"ts_ns\":\"{ts_ns}\"}}",
            audit.kind(),
            json_escape(reason)
        ),
        BackpressureAudit::SlowConsumerSpilled {
            pending,
            high_water,
            spilled,
            reason,
            ts_ns,
        } => format!(
            "{{\"kind\":\"{}\",\"pending\":\"{pending}\",\"high_water\":\"{high_water}\",\
             \"spilled\":\"{spilled}\",\"reason\":\"{}\",\"ts_ns\":\"{ts_ns}\"}}",
            audit.kind(),
            json_escape(reason)
        ),
        BackpressureAudit::SpillFailedAdmitted {
            pending,
            reason,
            ts_ns,
        } => format!(
            "{{\"kind\":\"{}\",\"pending\":\"{pending}\",\"reason\":\"{}\",\"ts_ns\":\"{ts_ns}\"}}",
            audit.kind(),
            json_escape(reason)
        ),
        BackpressureAudit::SamplingReduced {
            rows,
            dwell_samples,
            factor_permille,
            poll_interval_ms,
            ts_ns,
        } => format!(
            "{{\"kind\":\"{}\",\"rows\":\"{rows}\",\"dwell_samples\":\"{dwell_samples}\",\
             \"factor_permille\":\"{factor_permille}\",\"poll_interval_ms\":\"{poll_interval_ms}\",\
             \"ts_ns\":\"{ts_ns}\"}}",
            audit.kind()
        ),
        BackpressureAudit::SamplingRestored {
            rows,
            dwell_samples,
            factor_permille,
            poll_interval_ms,
            ts_ns,
        } => format!(
            "{{\"kind\":\"{}\",\"rows\":\"{rows}\",\"dwell_samples\":\"{dwell_samples}\",\
             \"factor_permille\":\"{factor_permille}\",\"poll_interval_ms\":\"{poll_interval_ms}\",\
             \"ts_ns\":\"{ts_ns}\"}}",
            audit.kind()
        ),
        BackpressureAudit::AcquisitionAlarm {
            rows,
            high_water_rows,
            factor_permille,
            reason,
            ts_ns,
        } => format!(
            "{{\"kind\":\"{}\",\"rows\":\"{rows}\",\"high_water_rows\":\"{high_water_rows}\",\
             \"factor_permille\":\"{factor_permille}\",\"reason\":\"{}\",\"ts_ns\":\"{ts_ns}\"}}",
            audit.kind(),
            json_escape(reason)
        ),
    }
}

/// 审计环形缓冲（线程安全，有界；超限淘汰最旧并计数，**绝不阻塞**）。
pub struct AuditLog {
    inner: Mutex<AuditInner>,
}

#[derive(Debug, Default)]
struct AuditInner {
    entries: VecDeque<BackpressureAudit>,
    capacity: usize,
    emitted: u64,
    dropped: u64,
}

impl AuditLog {
    /// 构造（`capacity` 至少 1）。
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let inner = AuditInner {
            capacity: capacity.max(1),
            ..AuditInner::default()
        };
        Self {
            inner: Mutex::new(inner),
        }
    }

    /// 记录一条审计（有界：超限淘汰最旧并 `dropped += 1`）。
    pub fn record(&self, event: BackpressureAudit) {
        let mut guard = lock(&self.inner);
        guard.emitted = guard.emitted.saturating_add(1);
        if guard.entries.len() >= guard.capacity {
            guard.entries.pop_front();
            guard.dropped = guard.dropped.saturating_add(1);
        }
        guard.entries.push_back(event);
    }

    /// 取出并清空全部审计（供上报 / 测试断言）。
    pub fn drain(&self) -> Vec<BackpressureAudit> {
        let mut guard = lock(&self.inner);
        guard.entries.drain(..).collect()
    }

    /// 快照（不清空）。
    pub fn snapshot(&self) -> Vec<BackpressureAudit> {
        lock(&self.inner).entries.iter().cloned().collect()
    }

    /// 当前缓冲条数。
    pub fn len(&self) -> usize {
        lock(&self.inner).entries.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 累计产生条数。
    pub fn emitted(&self) -> u64 {
        lock(&self.inner).emitted
    }

    /// 因缓冲满被淘汰的条数（可观测：说明消费审计的一方变慢了）。
    pub fn dropped(&self) -> u64 {
        lock(&self.inner).dropped
    }

    /// 是否包含指定类型的事件。
    #[must_use]
    pub fn contains_kind(&self, kind: &str) -> bool {
        self.snapshot().iter().any(|e| e.kind() == kind)
    }
}

impl Default for AuditLog {
    fn default() -> Self {
        Self::new(DEFAULT_AUDIT_CAPACITY)
    }
}

impl fmt::Debug for AuditLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let guard = lock(&self.inner);
        f.debug_struct("AuditLog")
            .field("len", &guard.entries.len())
            .field("capacity", &guard.capacity)
            .field("emitted", &guard.emitted)
            .field("dropped", &guard.dropped)
            .finish_non_exhaustive()
    }
}

// ==================== 5. 慢消费者保护（发送队列水位 + 落盘降级） ====================

/// 默认发送高水位（设计文档 §4：QoS1 RTT 20ms 时 32 条在途 ≈ 1600 条/秒）。
pub const DEFAULT_SEND_HIGH_WATER: usize = 32;

/// 默认发送硬上限（= rumqttc 请求通道容量 64）。
pub const DEFAULT_SEND_HARD_LIMIT: usize = 64;

/// 一条待发送（已提交未确认）的上报。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSend {
    /// 批次序号（`batch_seq`，构成幂等键）。
    pub seq: u64,
    /// 负载。
    pub payload: Vec<u8>,
    /// 入队时刻（纳秒）。
    pub enqueued_ns: i64,
}

impl PendingSend {
    /// 构造。
    #[must_use]
    pub fn new(seq: u64, payload: Vec<u8>, enqueued_ns: i64) -> Self {
        Self {
            seq,
            payload,
            enqueued_ns,
        }
    }

    /// 负载字节数。
    #[must_use]
    pub fn payload_len(&self) -> usize {
        self.payload.len()
    }
}

/// 落盘降级结果。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpillResult {
    /// 被落盘接管的条数。
    pub accepted: usize,
    /// 落盘失败、需调用方处理的条数（**必须审计，绝不静默**）。
    pub rejected: usize,
}

/// 落盘降级接收端（生产实现见 [`QueueSpillSink`]）。
pub trait SpillSink: Send + Sync {
    /// 接管一批被降级的待发送数据，返回接管的条数。
    ///
    /// 契约：**永不阻塞**调用方超过一次有界写入；无法接管的条数必须以
    /// `rejected` 返回（由调用方交还上层并审计）。
    fn spill(&self, items: Vec<PendingSend>) -> SpillResult;
}

/// 生产实现：把降级数据交给 [`OfflineQueue`]。
///
/// 入队成功后即视为数据安全（内存或磁盘），随后**尽最大努力** `flush()` 强制落盘，
/// 避免慢消费者积压驻留内存；`flush` 失败仅记 `tracing::warn`（数据仍在队列中，未丢失）。
pub struct QueueSpillSink {
    queue: Arc<OfflineQueue>,
}

impl QueueSpillSink {
    /// 构造。
    #[must_use]
    pub fn new(queue: Arc<OfflineQueue>) -> Self {
        Self { queue }
    }

    /// 底层队列。
    #[must_use]
    pub fn queue(&self) -> &Arc<OfflineQueue> {
        &self.queue
    }
}

impl SpillSink for QueueSpillSink {
    fn spill(&self, items: Vec<PendingSend>) -> SpillResult {
        let mut result = SpillResult::default();
        let mut need_flush = false;
        for item in items {
            match self.queue.enqueue(item.payload) {
                Ok(_) => {
                    result.accepted = result.accepted.saturating_add(1);
                    need_flush = true;
                }
                Err(err) => {
                    result.rejected = result.rejected.saturating_add(1);
                    tracing::warn!(
                        batch_seq = item.seq,
                        error = %err,
                        "slow-consumer spill rejected: queue refused the batch \
                         (data returned to caller, never silently dropped)"
                    );
                }
            }
        }
        if need_flush {
            // 尽最大努力落盘：失败不影响「数据已安全入队」的事实（仍在内存中）。
            if let Err(err) = self.queue.flush() {
                tracing::warn!(error = %err, "slow-consumer spill flush failed; batch stays in memory");
            }
        }
        result
    }
}

/// `push` 结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushOutcome {
    /// 已入发送队列（内存有界）。
    Admitted,
    /// 已落盘降级（内存未增长）。
    Spilled {
        /// 落盘条数。
        count: usize,
    },
    /// 落盘也失败 → 数据**交还调用方**（`returned` 非空），并已记审计。
    Rejected {
        /// 交还的批次（调用方负责计数 / 告警 / 重试）。
        returned: Vec<PendingSend>,
    },
}

impl PushOutcome {
    /// 是否进入内存发送队列。
    #[must_use]
    pub fn is_admitted(&self) -> bool {
        matches!(self, Self::Admitted)
    }

    /// 是否被落盘降级。
    #[must_use]
    pub fn is_spilled(&self) -> bool {
        matches!(self, Self::Spilled { .. })
    }

    /// 是否被拒绝（数据已交还，未静默丢弃）。
    #[must_use]
    pub fn is_rejected(&self) -> bool {
        matches!(self, Self::Rejected { .. })
    }
}

/// 发送队列统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SendQueueStats {
    /// 入内存条数。
    pub admitted: u64,
    /// 落盘降级条数。
    pub spilled: u64,
    /// 被拒绝（交还调用方）条数。
    pub rejected: u64,
    /// 当前在途（已提交未确认）条数。
    pub pending: usize,
    /// 当前在途字节数。
    pub bytes: usize,
}

/// 慢消费者保护：有界发送队列（水位 = 已提交未确认条数）。
///
/// **永不阻塞采集路径**：`push` 只做一次水位比较 + 一次有界落盘调用；
/// 超限即降级落盘，硬上限且落盘失败则把数据交还调用方并审计。
pub struct SendQueue {
    /// 待提交网络层（已占水位）。
    ready: VecDeque<PendingSend>,
    /// 已提交、等 PUBACK/PUBCOMP（已占水位）。
    sent: VecDeque<PendingSend>,
    bytes: usize,
    cfg: WaterMarkConfig,
    spill: Arc<dyn SpillSink>,
    audit: Arc<AuditLog>,
    clock: Arc<dyn Clock>,
    admitted: u64,
    spilled: u64,
    rejected: u64,
    high_entered: bool,
}

impl SendQueue {
    /// 构造（`cfg` 用 [`WaterMarkConfig::new`] 保证高水位 ≤ 硬上限 4/5）。
    #[must_use]
    pub fn new(
        cfg: WaterMarkConfig,
        spill: Arc<dyn SpillSink>,
        audit: Arc<AuditLog>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            ready: VecDeque::new(),
            sent: VecDeque::new(),
            bytes: 0,
            cfg,
            spill,
            audit,
            clock,
            admitted: 0,
            spilled: 0,
            rejected: 0,
            high_entered: false,
        }
    }

    /// 默认水位（32 / 64，设计文档 §4）构造。
    #[must_use]
    pub fn with_defaults(
        spill: Arc<dyn SpillSink>,
        audit: Arc<AuditLog>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self::new(
            WaterMarkConfig::new(DEFAULT_SEND_HIGH_WATER, DEFAULT_SEND_HARD_LIMIT),
            spill,
            audit,
            clock,
        )
    }

    /// 在途条数（水位口径 = 已提交未确认）。
    #[must_use]
    pub fn pending(&self) -> usize {
        self.ready.len().saturating_add(self.sent.len())
    }

    /// 在途字节数。
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// 水位快照。
    #[must_use]
    pub fn gauge(&self) -> Gauge {
        Gauge::new(self.pending(), self.bytes)
    }

    /// 当前水位等级。
    #[must_use]
    pub fn level(&self) -> WaterLevel {
        self.cfg.level(self.gauge())
    }

    /// 统计。
    #[must_use]
    pub fn stats(&self) -> SendQueueStats {
        SendQueueStats {
            admitted: self.admitted,
            spilled: self.spilled,
            rejected: self.rejected,
            pending: self.pending(),
            bytes: self.bytes,
        }
    }

    /// 提交一条待发送数据（**永不阻塞**）。
    pub fn push(&mut self, item: PendingSend) -> PushOutcome {
        let gauge = self.gauge();
        match self.cfg.level(gauge) {
            WaterLevel::Normal => {
                self.note_level(gauge);
                self.admit(item);
                PushOutcome::Admitted
            }
            WaterLevel::High => {
                self.note_level(gauge);
                self.spill_or_admit(item, gauge)
            }
            WaterLevel::Critical => {
                self.note_level(gauge);
                self.spill_or_reject(item, gauge)
            }
        }
    }

    /// 取出最多 `max` 条交给网络层发布（`ready` → `sent`，**水位占用不变**）。
    pub fn take_ready(&mut self, max: usize) -> Vec<PendingSend> {
        let mut out = Vec::new();
        while out.len() < max {
            match self.ready.pop_front() {
                Some(item) => {
                    self.sent.push_back(item.clone());
                    out.push(item);
                }
                None => break,
            }
        }
        out
    }

    /// 卸下**全部**在途条目（热重建 / 停机前的**数据安全收口**）。
    ///
    /// `SendQueue` 的在途条目（`ready` + `sent`）只存在于内存：本类型没有 `Drop`
    /// 实现（不会自动落盘），直接丢弃出口句柄 = 静默丢数据。北向出口被删除 /
    /// 被重建前必须先调本方法把数据取走，再回灌 [`OfflineQueue`](crate::offline_queue::OfflineQueue)
    /// ——这样「超限落盘 → 幂等补发」的闭环不断裂。
    ///
    /// 返回顺序按**入队先后**（`sent` 早于 `ready`）；调用后水位归零。
    pub fn drain_all(&mut self) -> Vec<PendingSend> {
        let mut out = Vec::with_capacity(self.sent.len().saturating_add(self.ready.len()));
        out.extend(self.sent.drain(..));
        out.extend(self.ready.drain(..));
        self.bytes = 0;
        self.high_entered = false;
        self.note_level(self.gauge());
        out
    }

    /// PUBACK/PUBCOMP 到达：从在途窗口移除 `count` 条（按提交顺序）。
    ///
    /// 返回实际移除条数（不超过 `sent` 长度）。
    pub fn confirm(&mut self, count: usize) -> usize {
        let mut removed = 0;
        while removed < count {
            match self.sent.pop_front() {
                Some(item) => {
                    self.bytes = self.bytes.saturating_sub(item.payload_len());
                    removed = removed.saturating_add(1);
                }
                None => break,
            }
        }
        if removed > 0 {
            self.note_level(self.gauge());
        }
        removed
    }

    /// 发布失败回灌（`sent` → `ready` 头部），水位占用不变。
    ///
    /// `items` 有两种来源：
    /// - 来自 [`Self::take_ready`] 的**已取出**条目——它们此刻同时存在于 `sent`，
    ///   必须先按 `seq` 从 `sent` 移除，否则同一批次被**两层重复计数**，
    ///   使 `pending()` / `bytes()` 水位虚高（⇒ 过早降级 / 误拒）；
    /// - 来自补发路径的**从未取出**条目——`sent` 中无同 `seq` 项，移除为 no-op。
    ///
    /// 两种来源都只做「移除同 `seq` 条目 + `push_front` 回 `ready` 头部」，
    /// `bytes` 字段自始至终不变（条目在 `admit` 时已计入，`take_ready` 不改动）。
    ///
    /// 为保持回灌后 `ready` 内的相对顺序与出队前一致，按 `items` **逆序** `push_front`
    /// （`[a, b, c]` → 依次压入 `c, b, a` → 头部序列仍为 `[a, b, c]`）。
    pub fn requeue_failed(&mut self, items: Vec<PendingSend>) {
        for item in items.into_iter().rev() {
            if let Some(pos) = self.sent.iter().position(|s| s.seq == item.seq) {
                self.sent.remove(pos);
            }
            self.ready.push_front(item);
        }
        self.note_level(self.gauge());
    }

    /// 高水位：优先落盘降级；落盘不可用则退回内存（仍受硬上限保护）+ 审计。
    ///
    /// 说明：降级路径需保留一份副本以便「落盘失败时退回内存」，故入参在此克隆一次
    /// （仅发生在高水位降级路径，300 B/条量级）。
    fn spill_or_admit(&mut self, item: PendingSend, gauge: Gauge) -> PushOutcome {
        let result = self.spill.spill(vec![item.clone()]);
        if result.accepted > 0 {
            self.spilled = self
                .spilled
                .saturating_add(u64::try_from(result.accepted).unwrap_or(u64::MAX));
            self.audit.record(BackpressureAudit::SlowConsumerSpilled {
                pending: u64::try_from(gauge.rows).unwrap_or(u64::MAX),
                high_water: u64::try_from(self.cfg.high_water_rows).unwrap_or(u64::MAX),
                spilled: u64::try_from(result.accepted).unwrap_or(u64::MAX),
                reason: format!(
                    "send queue above high water ({}/{}): degraded to disk",
                    gauge.rows, self.cfg.high_water_rows
                ),
                ts_ns: self.clock.now_ns(),
            });
            return PushOutcome::Spilled {
                count: result.accepted,
            };
        }
        self.audit.record(BackpressureAudit::SpillFailedAdmitted {
            pending: u64::try_from(gauge.rows).unwrap_or(u64::MAX),
            reason: "spill sink unavailable at high water; batch kept in bounded memory \
                     (hard limit still enforced)"
                .to_string(),
            ts_ns: self.clock.now_ns(),
        });
        self.admit_requeued(item);
        PushOutcome::Admitted
    }

    /// 硬上限：必须落盘；落盘失败则把数据交还调用方 + 审计（绝不静默）。
    fn spill_or_reject(&mut self, item: PendingSend, gauge: Gauge) -> PushOutcome {
        let seq = item.seq;
        let payload_len = item.payload_len();
        let result = self.spill.spill(vec![item.clone()]);
        if result.accepted > 0 {
            self.spilled = self
                .spilled
                .saturating_add(u64::try_from(result.accepted).unwrap_or(u64::MAX));
            self.audit.record(BackpressureAudit::SlowConsumerSpilled {
                pending: u64::try_from(gauge.rows).unwrap_or(u64::MAX),
                high_water: u64::try_from(self.cfg.high_water_rows).unwrap_or(u64::MAX),
                spilled: u64::try_from(result.accepted).unwrap_or(u64::MAX),
                reason: format!(
                    "send queue at hard limit ({}/{}): degraded to disk",
                    gauge.rows, self.cfg.hard_limit_rows
                ),
                ts_ns: self.clock.now_ns(),
            });
            return PushOutcome::Spilled {
                count: result.accepted,
            };
        }
        self.rejected = self.rejected.saturating_add(1);
        self.audit.record(BackpressureAudit::OverflowRejected {
            seq,
            rows: u64::try_from(gauge.rows).unwrap_or(u64::MAX),
            bytes: u64::try_from(gauge.bytes).unwrap_or(u64::MAX),
            limit: u64::try_from(self.cfg.hard_limit_rows).unwrap_or(u64::MAX),
            reason: format!(
                "send queue at hard limit ({}/{}) and spill sink rejected the batch \
                 ({} bytes); data returned to caller, never silently dropped",
                gauge.rows, self.cfg.hard_limit_rows, payload_len
            ),
            ts_ns: self.clock.now_ns(),
        });
        PushOutcome::Rejected {
            returned: vec![item],
        }
    }

    /// 内存收件（提交路径）。
    fn admit(&mut self, item: PendingSend) {
        self.admitted = self.admitted.saturating_add(1);
        self.bytes = self.bytes.saturating_add(item.payload_len());
        self.ready.push_back(item);
    }

    /// 内存收件（回灌 / 降级退回路径，不计入 admitted）。
    fn admit_requeued(&mut self, item: PendingSend) {
        self.bytes = self.bytes.saturating_add(item.payload_len());
        self.ready.push_back(item);
    }

    /// 高水位进入 / 退出的一次性留痕（迟滞：进入后需回落才再次记录）。
    fn note_level(&mut self, gauge: Gauge) {
        let is_high = matches!(
            self.cfg.level(gauge),
            WaterLevel::High | WaterLevel::Critical
        );
        if is_high && !self.high_entered {
            self.high_entered = true;
            self.audit.record(BackpressureAudit::HighWaterEntered {
                rows: u64::try_from(gauge.rows).unwrap_or(u64::MAX),
                bytes: u64::try_from(gauge.bytes).unwrap_or(u64::MAX),
                high_water_rows: u64::try_from(self.cfg.high_water_rows).unwrap_or(u64::MAX),
                ts_ns: self.clock.now_ns(),
            });
        } else if !is_high && self.high_entered {
            self.high_entered = false;
            self.audit.record(BackpressureAudit::HighWaterCleared {
                rows: u64::try_from(gauge.rows).unwrap_or(u64::MAX),
                bytes: u64::try_from(gauge.bytes).unwrap_or(u64::MAX),
                ts_ns: self.clock.now_ns(),
            });
        }
    }
}

impl fmt::Debug for SendQueue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SendQueue")
            .field("ready", &self.ready.len())
            .field("sent", &self.sent.len())
            .field("bytes", &self.bytes)
            .field("admitted", &self.admitted)
            .field("spilled", &self.spilled)
            .field("rejected", &self.rejected)
            .finish_non_exhaustive()
    }
}

// ==================== 6. 采集侧背压（持续高位 → 降采样 / 告警） ====================

/// 采集侧背压配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquisitionGovernorConfig {
    /// 高水位行数（≥ 此值且持续 → 降采样）。
    pub high_water_rows: usize,
    /// 低水位行数（迟滞下界：≤ 此值且持续 → 恢复采样）。
    pub low_water_rows: usize,
    /// 连续多少次采样达高水位才降采样。
    pub sustain_samples: u32,
    /// 连续多少次采样低于低水位才恢复。
    pub recover_samples: u32,
    /// 每次降采样的步长（千分比，500 = 频率减半 / 周期翻倍）。
    pub reduce_step_permille: u32,
    /// 降采样下限（千分比，250 = 最低降到原频率的 1/4）。
    pub min_factor_permille: u32,
    /// 基准轮询周期（毫秒）。
    pub base_poll_interval_ms: u64,
}

impl Default for AcquisitionGovernorConfig {
    /// 默认：高水位取内存队列默认高位 8192，低水位取其一半，连续 3 次降 / 5 次恢复。
    fn default() -> Self {
        Self {
            high_water_rows: DEFAULT_MEM_HIGH_WATER_ROWS,
            low_water_rows: DEFAULT_MEM_HIGH_WATER_ROWS / 2,
            sustain_samples: 3,
            recover_samples: 5,
            reduce_step_permille: 500,
            min_factor_permille: 250,
            base_poll_interval_ms: 100,
        }
    }
}

/// 采样动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SamplingAction {
    /// 维持当前频率。
    Hold,
    /// 已降采样（周期放大）。
    Reduced,
    /// 已恢复（周期回落）。
    Restored,
}

/// 一次采样（一轮调度）后的决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SamplingDecision {
    /// 本轮动作。
    pub action: SamplingAction,
    /// 生效频率因子（千分比，1000 = 原频率）。
    pub factor_permille: u32,
    /// **下一轮**应采用的轮询周期（毫秒）。
    pub poll_interval_ms: u64,
    /// 本轮观测到的队列行数。
    pub rows: u64,
    /// 本轮结束时的连续采样计数。
    pub dwell_samples: u32,
}

/// 采集侧背压调控器：队列持续高位 → 逐级降采样；带迟滞恢复；降到底仍高位 → 告警。
pub struct AcquisitionGovernor {
    cfg: AcquisitionGovernorConfig,
    factor_permille: u32,
    high_dwell: u32,
    low_dwell: u32,
    audit: Arc<AuditLog>,
    clock: Arc<dyn Clock>,
}

impl AcquisitionGovernor {
    /// 构造（起始频率因子 = 1000‰，即基准周期）。
    #[must_use]
    pub fn new(
        cfg: AcquisitionGovernorConfig,
        audit: Arc<AuditLog>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            cfg,
            factor_permille: 1_000,
            high_dwell: 0,
            low_dwell: 0,
            audit,
            clock,
        }
    }

    /// 当前频率因子（千分比）。
    #[must_use]
    pub fn factor_permille(&self) -> u32 {
        self.factor_permille
    }

    /// 当前轮询周期（毫秒）。
    #[must_use]
    pub fn poll_interval_ms(&self) -> u64 {
        Self::interval_for(self.cfg.base_poll_interval_ms, self.factor_permille)
    }

    /// 周期换算：`base × 1000 / factor`，最小 1ms（**绝不除零**）。
    #[must_use]
    pub fn interval_for(base_poll_interval_ms: u64, factor_permille: u32) -> u64 {
        if factor_permille == 0 {
            return base_poll_interval_ms.max(1);
        }
        let scaled = u128::from(base_poll_interval_ms).saturating_mul(1_000);
        let out = scaled
            .checked_div(u128::from(factor_permille))
            .and_then(|v| u64::try_from(v).ok());
        out.unwrap_or(base_poll_interval_ms).max(1)
    }

    /// 观测一次队列行数（每轮调度调用一次），返回下一轮的采样决策。
    pub fn observe(&mut self, rows: usize) -> SamplingDecision {
        let rows_u64 = u64::try_from(rows).unwrap_or(u64::MAX);
        if rows >= self.cfg.high_water_rows {
            self.low_dwell = 0;
            self.high_dwell = self.high_dwell.saturating_add(1);
            return self.on_sustained_high(rows_u64);
        }
        if rows <= self.cfg.low_water_rows {
            self.high_dwell = 0;
            self.low_dwell = self.low_dwell.saturating_add(1);
            return self.on_sustained_low(rows_u64);
        }
        // 中性区（低水位与高水位之间）：不动作，也不清零计数（避免抖动被抹平）。
        SamplingDecision {
            action: SamplingAction::Hold,
            factor_permille: self.factor_permille,
            poll_interval_ms: self.poll_interval_ms(),
            rows: rows_u64,
            dwell_samples: self.high_dwell.max(self.low_dwell),
        }
    }

    /// 持续高位：达到 `sustain_samples` 即降一级；已到下限则告警。
    fn on_sustained_high(&mut self, rows: u64) -> SamplingDecision {
        if self.high_dwell < self.cfg.sustain_samples.max(1) {
            return self.decision(SamplingAction::Hold, rows, self.high_dwell);
        }
        let floor = self.cfg.min_factor_permille.min(1_000);
        if self.factor_permille <= floor {
            self.high_dwell = 0;
            self.audit.record(BackpressureAudit::AcquisitionAlarm {
                rows,
                high_water_rows: u64::try_from(self.cfg.high_water_rows).unwrap_or(u64::MAX),
                factor_permille: self.factor_permille,
                reason: format!(
                    "queue stays at/above high water ({}) and sampling factor already at floor \
                     ({} permille): operator action required",
                    self.cfg.high_water_rows, self.factor_permille
                ),
                ts_ns: self.clock.now_ns(),
            });
            return self.decision(SamplingAction::Hold, rows, self.high_dwell);
        }
        let scaled = u128::from(self.factor_permille)
            .saturating_mul(u128::from(self.cfg.reduce_step_permille))
            .checked_div(PERMILLE)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(floor);
        self.factor_permille = scaled.max(floor);
        self.high_dwell = 0;
        self.audit.record(BackpressureAudit::SamplingReduced {
            rows,
            dwell_samples: self.cfg.sustain_samples,
            factor_permille: self.factor_permille,
            poll_interval_ms: self.poll_interval_ms(),
            ts_ns: self.clock.now_ns(),
        });
        self.decision(SamplingAction::Reduced, rows, self.cfg.sustain_samples)
    }

    /// 持续低位：达到 `recover_samples` 即恢复一级（翻倍，上限 1000‰）。
    fn on_sustained_low(&mut self, rows: u64) -> SamplingDecision {
        if self.low_dwell < self.cfg.recover_samples.max(1) || self.factor_permille >= 1_000 {
            return self.decision(SamplingAction::Hold, rows, self.low_dwell);
        }
        self.factor_permille = self.factor_permille.saturating_mul(2).min(1_000);
        self.low_dwell = 0;
        self.audit.record(BackpressureAudit::SamplingRestored {
            rows,
            dwell_samples: self.cfg.recover_samples,
            factor_permille: self.factor_permille,
            poll_interval_ms: self.poll_interval_ms(),
            ts_ns: self.clock.now_ns(),
        });
        self.decision(SamplingAction::Restored, rows, self.cfg.recover_samples)
    }

    fn decision(&self, action: SamplingAction, rows: u64, dwell_samples: u32) -> SamplingDecision {
        SamplingDecision {
            action,
            factor_permille: self.factor_permille,
            poll_interval_ms: self.poll_interval_ms(),
            rows,
            dwell_samples,
        }
    }
}

impl fmt::Debug for AcquisitionGovernor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AcquisitionGovernor")
            .field("factor_permille", &self.factor_permille)
            .field("high_dwell", &self.high_dwell)
            .field("low_dwell", &self.low_dwell)
            .field("poll_interval_ms", &self.poll_interval_ms())
            .finish_non_exhaustive()
    }
}

// ==================== 7. 入队入口（背压 + 溢出审计 + 数据交还） ====================

/// 入队结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmitOutcome {
    /// 已入队（返回分配的 `batch_seq`，构成幂等键）。
    Admitted {
        /// 分配的批次序号。
        seq: u64,
    },
    /// 触达硬上限被拒绝：**负载原样交还**（调用方负责计数 / 告警 / 重试）。
    Rejected {
        /// 交还的负载（**不为空**：绝不静默丢数据）。
        payload: Vec<u8>,
        /// 拒绝原因（已同时记审计）。
        reason: String,
    },
}

/// 带背压的入队入口（替代裸 `OfflineQueue::enqueue` 的推荐接线点）。
///
/// - `Normal` → 入队；
/// - `High` → 入队（队列内部自行降级落盘），失败则交还负载 + 审计；
/// - `Critical` → **不调用入队**，直接交还负载 + [`BackpressureAudit::OverflowRejected`]。
///
/// 说明：为在失败路径上「原样交还负载」，入队前会保留一份副本（常规路径副本随即释放，
/// 仅 300 B/条量级的 memcpy，换取「绝不丢数据」的可验证性）。
pub fn enqueue_with_backpressure(
    queue: &OfflineQueue,
    payload: Vec<u8>,
    audit: &AuditLog,
    clock: &dyn Clock,
) -> AdmitOutcome {
    let marks = WaterMarkConfig::from_queue_config(queue.config());
    // 字节维度不可从队列外部读取 → 用行数判定（队列内部仍按字节夹紧）。
    let gauge = Gauge::new(queue.pending_mem(), 0);
    match marks.action(gauge) {
        BackpressureAction::Admit | BackpressureAction::Degrade { .. } => {
            let backup = payload.clone();
            match queue.enqueue(payload) {
                Ok(seq) => AdmitOutcome::Admitted { seq },
                Err(err) => {
                    let reason = err.to_string();
                    audit.record(BackpressureAudit::OverflowRejected {
                        seq: 0,
                        rows: u64::try_from(gauge.rows).unwrap_or(u64::MAX),
                        bytes: 0,
                        limit: u64::try_from(marks.hard_limit_rows).unwrap_or(u64::MAX),
                        reason: reason.clone(),
                        ts_ns: clock.now_ns(),
                    });
                    AdmitOutcome::Rejected {
                        payload: backup,
                        reason,
                    }
                }
            }
        }
        BackpressureAction::Reject { reason } => {
            audit.record(BackpressureAudit::OverflowRejected {
                seq: 0,
                rows: u64::try_from(gauge.rows).unwrap_or(u64::MAX),
                bytes: 0,
                limit: u64::try_from(marks.hard_limit_rows).unwrap_or(u64::MAX),
                reason: reason.clone(),
                ts_ns: clock.now_ns(),
            });
            AdmitOutcome::Rejected { payload, reason }
        }
    }
}

// ==================== 工具 ====================

/// 锁中毒恢复（取回内部数据，**绝不 panic**）。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => PoisonError::into_inner(poisoned),
    }
}

/// JSON 字符串转义（最小实现：反斜杠 / 双引号 / 控制字符）。
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::offline_queue::ManualClock;
    use std::sync::atomic::AtomicUsize;

    /// 测试用落盘接收端：可切换「接受 / 拒绝」并计数。
    struct RecordingSpillSink {
        accept: AtomicBool,
        accepted: AtomicUsize,
        rejected: AtomicUsize,
        items: Mutex<Vec<PendingSend>>,
    }

    impl RecordingSpillSink {
        fn new(accept: bool) -> Self {
            Self {
                accept: AtomicBool::new(accept),
                accepted: AtomicUsize::new(0),
                rejected: AtomicUsize::new(0),
                items: Mutex::new(Vec::new()),
            }
        }

        fn accepted(&self) -> usize {
            self.accepted.load(Ordering::SeqCst)
        }

        fn rejected(&self) -> usize {
            self.rejected.load(Ordering::SeqCst)
        }
    }
    impl SpillSink for RecordingSpillSink {
        fn spill(&self, items: Vec<PendingSend>) -> SpillResult {
            let count = items.len();
            if self.accept.load(Ordering::SeqCst) {
                lock(&self.items).extend(items);
                self.accepted.fetch_add(count, Ordering::SeqCst);
                return SpillResult {
                    accepted: count,
                    rejected: 0,
                };
            }
            self.rejected.fetch_add(count, Ordering::SeqCst);
            SpillResult {
                accepted: 0,
                rejected: count,
            }
        }
    }

    fn pending(seq: u64, size: usize, ts: i64) -> PendingSend {
        PendingSend::new(seq, vec![7u8; size], ts)
    }

    fn batch(seq: u64, gateway: &str) -> QueuedBatch {
        QueuedBatch {
            seq,
            gateway_id: gateway.to_string(),
            payload: vec![1u8, 2, 3],
            enqueued_ns: 1_000,
        }
    }

    // ---- 容量估算 ----

    #[test]
    fn capacity_fifty_devices_matches_plan_redline() {
        let est = estimate(&CapacityInput::reference_50_devices());
        // ⚠ 红线：50 设备 × 100ms = 500 条/秒，断网 1h = 1,800,000 条（不是 36,000）。
        assert_eq!(est.samples_per_sec, 500);
        assert_eq!(est.rows_per_hour, 1_800_000);
        assert_eq!(est.logical_bytes_per_sec, 150_000);
        assert_eq!(est.physical_bytes_per_sec, 225_000);
        assert_eq!(
            est.replay_seconds_one_hour_outage, 900,
            "1.8M / 2000 = 900s"
        );
        // 10 GiB / 150 KB/s ≈ 19.9h（设计文档 §2：≥ 18h）。
        assert!(est.disk_covered_seconds_logical >= 18 * 3_600);
        // 退化输入绝不 panic / 除零。
        assert_eq!(samples_per_second(50, 0), 0);
        assert_eq!(samples_per_second(0, 100), 0);
        assert_eq!(rows_in_window(50, 100, 0), 0);
        assert_eq!(disk_covered_seconds(0, DEFAULT_MAX_DB_BYTES), u64::MAX);
        assert_eq!(replay_seconds(100, 0), u64::MAX);
    }

    #[test]
    fn capacity_two_hundred_devices_matches_design_doc() {
        let est = estimate(&CapacityInput::reference_200_devices());
        assert_eq!(est.samples_per_sec, 2_000);
        assert_eq!(est.batches_per_sec, 2_000);
        assert_eq!(est.rows_per_hour, 7_200_000);
        assert_eq!(est.logical_bytes_per_sec, 600_000);
        assert_eq!(est.physical_bytes_per_sec, 900_000);
        // 7 天：2000 × 604800 = 1,209,600,000 条 × 300B ≈ 362.9 GB(dec) → 远超 10 GiB。
        assert_eq!(est.rows_in_retention, 1_209_600_000);
        assert_eq!(est.bytes_in_retention, 362_880_000_000);
        assert!(!est.fits_retention_window, "7 天数据量装不进 10GiB");
        assert_eq!(est.bottleneck, CapacityBottleneck::DiskCapacity);
        // 10 GiB 可支撑：逻辑 ≈ 4.97h，物理（WAL ×1.5）≈ 3.31h（设计文档 §2：3～4.5h）。
        assert_eq!(est.disk_covered_seconds_logical, 17_895);
        assert_eq!(est.disk_covered_seconds_physical, 11_930);
        assert!(est.disk_covered_seconds_physical >= 3 * 3_600);
        assert!(est.disk_covered_seconds_physical <= 5 * 3_600);
        assert_eq!(
            est.replay_seconds_one_hour_outage, 3_600,
            "7.2M / 2000 = 60min"
        );
    }

    #[test]
    fn capacity_batching_four_to_one_matches_design_doc() {
        let est = estimate(&CapacityInput::reference_200_devices_batched());
        assert_eq!(est.samples_per_sec, 2_000);
        assert_eq!(est.batches_per_sec, 500);
        assert_eq!(est.rows_per_hour, 1_800_000, "4:1 合批后 1h = 180 万批");
    }

    // ---- 幂等去重 ----

    #[test]
    fn idempotency_key_roundtrip_covers_u64_max() {
        let key = crate::offline_queue::idempotency_key("gw-1", 42);
        assert_eq!(parse_idempotency_key(&key), Some(("gw-1", 42)));
        assert_eq!(batch_seq_of_key("gw:18446744073709551615"), Some(u64::MAX));
        assert_eq!(parse_idempotency_key("no-separator"), None);
        assert_eq!(parse_idempotency_key("gw:abc"), None);
        assert_eq!(batch_seq_of_key(""), None);
    }

    #[test]
    fn replay_selection_skips_acked_batches_and_deduplicates() {
        let batches = vec![
            batch(3, "gw-1"),
            batch(1, "gw-1"),
            batch(2, "gw-1"),
            batch(2, "gw-1"),
        ];
        let selected = replay_selection(&batches, 1);
        let seqs: Vec<u64> = selected.iter().map(|b| b.seq).collect();
        assert_eq!(seqs, vec![2, 3], "跳过已确认 seq<=1，并按幂等键去重");
        assert!(replay_selection(&batches, 3).is_empty());
    }

    #[test]
    fn duplicate_resend_does_not_double_apply() {
        // 模拟：队列被 take 两次（未 Ack），消费端按幂等键去重入库。
        let batches = vec![batch(1, "gw-1"), batch(2, "gw-1"), batch(3, "gw-1")];
        let mut ledger = IdempotencyLedger::new("gw-1", 1024);
        let mut store: Vec<u64> = Vec::new();

        for round in 0..2 {
            let selected = replay_selection(&batches, ledger.high_water());
            assert_eq!(
                selected.len(),
                3,
                "第 {} 轮补发应返回全部未确认批次",
                round + 1
            );
            for item in selected {
                if ledger.apply_batch(item).is_applied() {
                    store.push(item.seq);
                }
            }
        }
        assert_eq!(store, vec![1, 2, 3], "重复补发不产生重复入库");
        assert_eq!(ledger.stats().applied_rows, 3);
        assert_eq!(ledger.stats().duplicate_rows, 3);
    }

    #[test]
    fn ack_loss_keeps_high_water_and_resend_still_dedups() {
        let sink = Arc::new(InMemoryAckSink::new(0));
        sink.set_fail(true); // 补发中途 Ack 落盘失败
        let mut controller = ReplayController::new("gw-1", sink.clone(), 0);
        let batches = vec![batch(1, "gw-1"), batch(2, "gw-1"), batch(3, "gw-1")];
        let mut store: Vec<u64> = Vec::new();

        for item in replay_selection(&batches, controller.high_water_mark()) {
            if controller.mark_sent(item.seq).is_applied() {
                store.push(item.seq);
            }
        }
        let ack_result = controller.ack(3);
        assert!(ack_result.is_err(), "Ack 落盘失败应返回 Err");
        assert_eq!(controller.high_water_mark(), 0, "位点未推进");
        assert_eq!(sink.persisted(), 0, "Ack 未落盘");

        // 重发：位点未推进 → 仍会重发同一批；消费端去重 → 仍不重复入库。
        for item in replay_selection(&batches, controller.high_water_mark()) {
            if controller.mark_sent(item.seq).is_applied() {
                store.push(item.seq);
            }
        }
        assert_eq!(store, vec![1, 2, 3], "重发仍不产生重复入库");

        // 恢复后 Ack 成功 → 位点推进，补发集合清空。
        sink.set_fail(false);
        assert_eq!(
            controller.ack(3).expect("ack"),
            AckOutcome::Advanced { from: 0, to: 3 }
        );
        assert_eq!(controller.high_water_mark(), 3);
        assert!(replay_selection(&batches, controller.high_water_mark()).is_empty());
    }

    #[test]
    fn ack_is_persisted_before_cursor_advances() {
        let sink = Arc::new(InMemoryAckSink::new(0));
        sink.set_fail(true);
        let mut cursor = AckCursor::new(sink.clone(), 0);
        assert!(cursor.persist_then_advance(7).is_err());
        assert_eq!(cursor.high_water_mark(), 0, "落盘失败 → 位点一步不动");
        assert_eq!(sink.calls(), 1, "确实尝试过落盘");

        sink.set_fail(false);
        assert!(cursor.persist_then_advance(7).is_ok());
        assert_eq!(cursor.high_water_mark(), 7);
        assert_eq!(sink.persisted(), 7, "先落 Ack 后推位点");
        // 幂等：重复确认（更旧位点）不回退。
        assert!(cursor.persist_then_advance(3).is_ok());
        assert_eq!(cursor.high_water_mark(), 7);
    }

    // ---- 水位 ----

    #[test]
    fn water_mark_clamp_matches_offline_queue_rule() {
        assert_eq!(clamp_high_water(100, 10), 8, "夹紧到 4/5");
        assert_eq!(clamp_high_water(0, 10), 1, "至少为 1");
        assert_eq!(clamp_high_water(1, 1), 1);

        let default_cfg = QueueConfig::new(std::path::PathBuf::from("queue.db"), "gw-1")
            .expect("default queue config");
        let marks = WaterMarkConfig::from_queue_config(&default_cfg);
        assert_eq!(marks.high_water_rows, 8_192);
        assert_eq!(marks.hard_limit_rows, 65_536);

        let mut tuned = default_cfg;
        tuned.max_mem_rows = 10;
        tuned.mem_high_water_rows = 100;
        assert_eq!(tuned.high_water_rows(), 8);
        assert_eq!(
            WaterMarkConfig::from_queue_config(&tuned).high_water_rows,
            8
        );
    }

    #[test]
    fn watermark_action_transitions_admit_degrade_reject() {
        let marks = WaterMarkConfig::new(8, 10);
        assert_eq!(marks.level(Gauge::new(0, 0)), WaterLevel::Normal);
        assert!(matches!(
            marks.action(Gauge::new(0, 0)),
            BackpressureAction::Admit
        ));
        assert_eq!(marks.level(Gauge::new(8, 0)), WaterLevel::High);
        assert!(matches!(
            marks.action(Gauge::new(8, 0)),
            BackpressureAction::Degrade { .. }
        ));
        assert_eq!(marks.level(Gauge::new(10, 0)), WaterLevel::Critical);
        assert!(matches!(
            marks.action(Gauge::new(10, 0)),
            BackpressureAction::Reject { .. }
        ));
        // 字节维度同样生效。
        let mut byte_marks = WaterMarkConfig::new(8, 10);
        byte_marks.high_water_bytes = 100;
        byte_marks.hard_limit_bytes = 200;
        assert_eq!(byte_marks.level(Gauge::new(0, 100)), WaterLevel::High);
        assert_eq!(byte_marks.level(Gauge::new(0, 200)), WaterLevel::Critical);
    }

    // ---- 慢消费者 ----

    #[test]
    fn slow_consumer_spills_to_disk_and_memory_stays_bounded() {
        let sink = Arc::new(RecordingSpillSink::new(true));
        let audit = Arc::new(AuditLog::new(64));
        let clock = Arc::new(ManualClock::new(1_000));
        let mut queue = SendQueue::new(
            WaterMarkConfig::new(4, 8),
            sink.clone(),
            audit.clone(),
            clock,
        );

        for seq in 1..=20u64 {
            let _ = queue.push(pending(seq, 300, 1_000));
            assert!(queue.pending() <= 8, "内存队列必须始终有界");
        }
        let stats = queue.stats();
        assert_eq!(stats.admitted, 4, "高水位以下才进内存");
        assert_eq!(stats.spilled, 16, "其余全部落盘降级");
        assert_eq!(stats.rejected, 0);
        assert_eq!(sink.accepted(), 16);
        assert!(audit.contains_kind("HighWaterEntered"), "水位触顶产生审计");
        assert!(audit.contains_kind("SlowConsumerSpilled"));

        // 发布 + PUBACK 后水位回落，重新接纳。
        let ready = queue.take_ready(10);
        assert_eq!(ready.len(), 4);
        assert_eq!(queue.confirm(4), 4);
        assert_eq!(queue.pending(), 0);
        assert!(audit.contains_kind("HighWaterCleared"));
        assert!(queue.push(pending(21, 300, 1_000)).is_admitted());
    }

    #[test]
    fn slow_consumer_rejection_hands_payload_back_and_audits() {
        let sink = Arc::new(RecordingSpillSink::new(false)); // 落盘也失败
        let audit = Arc::new(AuditLog::new(64));
        let clock = Arc::new(ManualClock::new(1_000));
        let mut queue = SendQueue::new(
            WaterMarkConfig::new(4, 8),
            sink.clone(),
            audit.clone(),
            clock,
        );

        let mut returned: Vec<PendingSend> = Vec::new();
        for seq in 1..=12u64 {
            if let PushOutcome::Rejected {
                returned: mut items,
            } = queue.push(pending(seq, 300, 1_000))
            {
                returned.append(&mut items);
            }
        }
        let stats = queue.stats();
        assert_eq!(stats.admitted, 4, "高水位以下进内存");
        assert_eq!(stats.rejected, 4, "硬上限后 4 条被拒绝");
        assert_eq!(returned.len(), 4, "被拒数据原样交还，绝不静默丢弃");
        assert!(returned.iter().all(|i| !i.payload.is_empty()));
        assert_eq!(sink.rejected(), 8);
        assert!(audit.contains_kind("OverflowRejected"), "拒绝必须审计");
        assert!(
            audit.contains_kind("SpillFailedAdmitted"),
            "降级不可用也留痕"
        );

        let rejected = audit
            .snapshot()
            .into_iter()
            .find(|e| e.kind() == "OverflowRejected");
        let json = audit_json(&rejected.expect("overflow audit"));
        assert!(
            json.contains("\"batch_seq\":\""),
            "大数红线：seq 编码为字符串"
        );
        assert!(json.contains("\"ts_ns\":\""));
    }

    // ---- 采集侧背压 ----

    #[test]
    fn acquisition_governor_reduces_then_restores_with_hysteresis() {
        let audit = Arc::new(AuditLog::new(64));
        let clock = Arc::new(ManualClock::new(1_000));
        let cfg = AcquisitionGovernorConfig {
            high_water_rows: 100,
            low_water_rows: 50,
            sustain_samples: 3,
            recover_samples: 5,
            reduce_step_permille: 500,
            min_factor_permille: 250,
            base_poll_interval_ms: 100,
        };
        let mut governor = AcquisitionGovernor::new(cfg, audit.clone(), clock);

        // 持续高位 → 逐级降采样（100ms → 200ms → 400ms）。
        assert_eq!(governor.observe(120).action, SamplingAction::Hold);
        assert_eq!(governor.observe(120).action, SamplingAction::Hold);
        let decision = governor.observe(120);
        assert_eq!(decision.action, SamplingAction::Reduced);
        assert_eq!(decision.factor_permille, 500);
        assert_eq!(decision.poll_interval_ms, 200);

        governor.observe(120);
        governor.observe(120);
        let decision = governor.observe(120);
        assert_eq!(decision.factor_permille, 250, "降到下限 250‰");
        assert_eq!(decision.poll_interval_ms, 400);

        // 已到下限仍高位 → 告警（且不再降）。
        governor.observe(120);
        governor.observe(120);
        let decision = governor.observe(120);
        assert_eq!(decision.action, SamplingAction::Hold);
        assert_eq!(decision.factor_permille, 250);
        assert!(
            audit.contains_kind("AcquisitionAlarm"),
            "降到底仍高位必须告警"
        );
        assert!(audit.contains_kind("SamplingReduced"));

        // 持续低位 → 迟滞恢复（250‰ → 500‰ → 1000‰）。
        for _ in 0..4 {
            assert_eq!(governor.observe(10).action, SamplingAction::Hold);
        }
        let decision = governor.observe(10);
        assert_eq!(decision.action, SamplingAction::Restored);
        assert_eq!(decision.factor_permille, 500);
        for _ in 0..5 {
            governor.observe(10);
        }
        assert_eq!(governor.factor_permille(), 1_000);
        assert_eq!(governor.poll_interval_ms(), 100);
        assert!(audit.contains_kind("SamplingRestored"));

        // 中性区不动作。
        assert_eq!(governor.observe(80).action, SamplingAction::Hold);
        assert_eq!(
            AcquisitionGovernor::interval_for(100, 0),
            100,
            "因子为 0 不 panic"
        );
    }

    // ---- 入队入口（真实 OfflineQueue 集成） ----

    #[test]
    fn enqueue_with_backpressure_returns_payload_and_audits_overflow() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("queue.db");
        let mut cfg = QueueConfig::new(db_path, "gw-bp").expect("config");
        cfg.max_mem_rows = 4;
        cfg.mem_high_water_rows = 1;
        cfg.max_mem_bytes = 4_096;
        let clock = Arc::new(ManualClock::new(1_000));
        let queue = OfflineQueue::open(cfg, clock.clone() as Arc<dyn Clock>).expect("open queue");
        // 模拟磁盘不可用：内存持续累积，直到触达硬上限。
        queue.set_persist_failure(true);

        let audit = AuditLog::new(64);
        let mut admitted = 0u64;
        let mut rejected_payloads: Vec<Vec<u8>> = Vec::new();
        for i in 0..6u64 {
            match enqueue_with_backpressure(&queue, vec![9u8; 8], &audit, clock.as_ref()) {
                AdmitOutcome::Admitted { seq } => {
                    admitted += 1;
                    assert_eq!(seq, i + 1, "batch_seq 单调递增");
                }
                AdmitOutcome::Rejected { payload, .. } => rejected_payloads.push(payload),
            }
        }
        assert_eq!(admitted, 4, "硬上限 4 行：前 4 条入队");
        assert_eq!(rejected_payloads.len(), 2, "超出部分被拒绝");
        assert!(
            rejected_payloads.iter().all(|p| p.len() == 8),
            "负载原样交还"
        );
        assert!(audit.contains_kind("OverflowRejected"), "水位触顶产生审计");
        assert_eq!(high_water_mark(&queue), 0, "未 Ack 前位点为 0");

        // Ack 语义：先落 Ack 后推位点（此处用内存 sink 验证顺序已在其它用例覆盖）。
        let sink = Arc::new(InMemoryAckSink::new(0));
        let mut controller = ReplayController::new("gw-bp", sink, 0);
        assert_eq!(
            controller.ack(2).expect("ack"),
            AckOutcome::Advanced { from: 0, to: 2 }
        );
        assert_eq!(controller.high_water_mark(), 2);
        assert!(controller.should_send(3));
        assert!(!controller.should_send(2));

        // 收尾：解除故障注入再关闭（close 会 flush 内存残留批次）。
        queue.set_persist_failure(false);
        queue.close().expect("close queue");
    }

    // ================= QA 独立验收（task 54 边界 / 反例） =================

    /// 审计事件构造（仅用于计数与顺序断言）。
    fn audit_ev(n: u64) -> BackpressureAudit {
        BackpressureAudit::HighWaterEntered {
            rows: n,
            bytes: 0,
            high_water_rows: 1,
            ts_ns: i64::try_from(n).unwrap_or(0),
        }
    }

    /// QA 边界：水位判定是 `>=` 语义（恰好等于阈值即升级），行 / 字节任一触达即升级。
    #[test]
    fn qa_water_level_exact_threshold_off_by_one() {
        let marks = WaterMarkConfig::new(4, 8);
        assert_eq!(marks.high_water_rows, 4, "4/5 夹紧后仍为 4");
        assert_eq!(marks.hard_limit_rows, 8);
        assert_eq!(marks.level(Gauge::new(3, 0)), WaterLevel::Normal);
        assert_eq!(
            marks.level(Gauge::new(4, 0)),
            WaterLevel::High,
            "恰好等于高水位即 High（>= 语义，非 >）"
        );
        assert_eq!(marks.level(Gauge::new(7, 0)), WaterLevel::High);
        assert_eq!(
            marks.level(Gauge::new(8, 0)),
            WaterLevel::Critical,
            "恰好等于硬上限即 Critical"
        );
        // 字节维度触达（默认字节上限为 usize::MAX）。
        assert_eq!(marks.level(Gauge::new(0, usize::MAX)), WaterLevel::Critical);

        let mut byte_marks = WaterMarkConfig::new(4, 8);
        byte_marks.high_water_bytes = 10;
        byte_marks.hard_limit_bytes = 20;
        assert_eq!(byte_marks.level(Gauge::new(0, 9)), WaterLevel::Normal);
        assert_eq!(byte_marks.level(Gauge::new(0, 10)), WaterLevel::High);
        assert_eq!(byte_marks.level(Gauge::new(0, 20)), WaterLevel::Critical);
        // 行数 Normal 但字节 High → 取更严者。
        assert_eq!(byte_marks.level(Gauge::new(1, 10)), WaterLevel::High);
        // 硬上限为 0 的退化配置：任何行数都是 Critical（不得 panic / 除零）。
        let degenerate = WaterMarkConfig::new(0, 0);
        assert_eq!(degenerate.hard_limit_rows, 1);
        assert_eq!(degenerate.level(Gauge::new(1, 0)), WaterLevel::Critical);
        assert_eq!(clamp_high_water(0, 0), 1);
    }

    /// QA 边界：审计环形缓冲满容量后 `dropped()` 必须真的计数，且保留最新条目。
    #[test]
    fn qa_audit_log_dropped_counts_after_capacity() {
        let log = AuditLog::new(3);
        for n in 0..5u64 {
            log.record(audit_ev(n));
        }
        assert_eq!(log.emitted(), 5);
        assert_eq!(log.dropped(), 2, "超容量淘汰必须计数");
        assert_eq!(log.len(), 3);
        let snap = log.snapshot();
        assert_eq!(snap.len(), 3);
        match &snap[0] {
            BackpressureAudit::HighWaterEntered { rows, .. } => {
                assert_eq!(*rows, 2, "保留最新 3 条（2/3/4）")
            }
            other => panic!("unexpected kind: {other:?}"),
        }
        assert_eq!(log.drain().len(), 3);
        assert!(log.is_empty());

        // 默认容量 4096：恰好写满不得计淘汰，第 4097 条起才淘汰。
        let big = AuditLog::default();
        for n in 0..DEFAULT_AUDIT_CAPACITY {
            big.record(audit_ev(u64::try_from(n).unwrap_or(0)));
        }
        assert_eq!(big.len(), DEFAULT_AUDIT_CAPACITY);
        assert_eq!(big.dropped(), 0, "恰好写满不得计淘汰");
        big.record(audit_ev(9_999));
        assert_eq!(big.dropped(), 1);
        assert_eq!(big.len(), DEFAULT_AUDIT_CAPACITY);
        assert_eq!(big.emitted(), DEFAULT_AUDIT_CAPACITY as u64 + 1);
    }

    /// QA 边界：`batch_seq = u64::MAX` 的键解析与位点饱和。
    #[test]
    fn qa_batch_seq_u64_max_and_high_water_saturation() {
        assert_eq!(
            parse_idempotency_key("gw:18446744073709551615"),
            Some(("gw", u64::MAX))
        );
        assert_eq!(batch_seq_of_key("gw:18446744073709551615"), Some(u64::MAX));
        // 2^64 溢出 / 空字段 / 缺分隔符一律不可解析（不得 panic）。
        assert_eq!(parse_idempotency_key("gw:18446744073709551616"), None);
        assert_eq!(parse_idempotency_key("gw:"), None);
        assert_eq!(parse_idempotency_key(":5"), None);
        assert_eq!(parse_idempotency_key("no-colon"), None);
        assert_eq!(parse_idempotency_key("gw:-1"), None);
        // 网关标识自身含冒号 → 取最后一节作序号。
        assert_eq!(parse_idempotency_key("a:b:7"), Some(("a:b", 7)));

        let mut ledger = IdempotencyLedger::new("gw", 8);
        assert!(ledger.apply_seq(1).is_applied());
        ledger.confirm_up_to(u64::MAX);
        assert_eq!(ledger.high_water(), u64::MAX);
        assert_eq!(ledger.stats().tracked, 0, "位点推进到 MAX 后窗口必须清空");
        assert!(
            ledger.apply_seq(u64::MAX).is_duplicate(),
            "seq <= high_water 一律判重复"
        );
        // 位点只增不减。
        ledger.confirm_up_to(1);
        assert_eq!(ledger.high_water(), u64::MAX);
    }

    /// QA 边界：去重窗口有界——被淘汰的旧 seq 会再次被判为「新数据」（已知折衷）。
    #[test]
    fn qa_ledger_window_is_bounded_and_evicts_oldest() {
        let mut ledger = IdempotencyLedger::new("gw", 2);
        assert!(ledger.apply_seq(1).is_applied());
        assert!(ledger.apply_seq(2).is_applied());
        assert!(ledger.apply_seq(3).is_applied());
        assert_eq!(ledger.stats().tracked, 2, "窗口必须有界");
        // 2 仍在窗口内 → 重复。
        assert_eq!(
            ledger.apply_seq(2),
            DedupOutcome::Duplicate {
                reason: DedupReason::AlreadyApplied
            }
        );
        // 1 已被淘汰 → 窗口外无法判定，再次入库（已知折衷，需靠位点推进兜住）。
        assert!(
            ledger.apply_seq(1).is_applied(),
            "窗口外条目无法判重（文档化折衷）"
        );
        assert_eq!(ledger.stats().duplicate_rows, 1);
    }

    /// QA 顺序：**先落 Ack 后推位点**——落盘失败时位点与去重窗口都不许动。
    #[test]
    fn qa_ack_persist_failure_freezes_cursor_and_window() {
        let sink = Arc::new(InMemoryAckSink::new(0));
        sink.set_fail(true);
        let mut ctrl = ReplayController::new("gw", sink.clone(), 0);
        assert!(ctrl.mark_sent(7).is_applied());

        assert!(ctrl.ack(7).is_err(), "Ack 落盘失败必须返回 Err");
        assert_eq!(sink.calls(), 1, "必须真的尝试落盘");
        assert_eq!(sink.persisted(), 0);
        assert_eq!(ctrl.high_water_mark(), 0, "位点一步不动");
        assert_eq!(ctrl.ledger().stats().high_water, 0, "去重窗口不得裁剪");
        assert!(
            ctrl.ledger().stats().tracked > 0,
            "窗口条目必须保留（否则重复会被当新数据）"
        );

        sink.set_fail(false);
        assert_eq!(
            ctrl.ack(7).expect("retry ack"),
            AckOutcome::Advanced { from: 0, to: 7 }
        );
        assert_eq!(ctrl.high_water_mark(), 7);
        assert_eq!(
            ctrl.ack(7).expect("idempotent ack"),
            AckOutcome::AlreadyAcked { cursor: 7 },
            "重复 Ack 幂等，不得重复落盘"
        );
        assert_eq!(sink.calls(), 2);
    }

    /// QA 回归（**RED**）：`requeue_failed` 只把条目推回 `ready` 头部，
    /// **却没有从 `sent` 移除**——同一批次同时计入两层，`pending()` / `bytes()`
    /// 被放大一倍（水位虚高 ⇒ 过早降级 / 误拒）。
    ///
    /// 契约（见 `requeue_failed` / `take_ready` 文档）：回灌 `sent → ready` 头部，
    /// **水位占用不变**。正确实现应 `self.sent` 中移除对应 `seq` 后再 `push_front`
    /// （或让 `take_ready` 不预先计入 `sent`）。
    #[test]
    fn qa_requeue_failed_must_keep_occupancy_unchanged() {
        let sink = Arc::new(RecordingSpillSink::new(true));
        let audit = Arc::new(AuditLog::new(8));
        let clock = Arc::new(ManualClock::new(1_000));
        let mut queue = SendQueue::new(WaterMarkConfig::new(4, 8), sink, audit, clock);

        assert!(queue.push(pending(1, 300, 1_000)).is_admitted());
        assert_eq!(queue.pending(), 1);
        let taken = queue.take_ready(1);
        assert_eq!(taken.len(), 1);
        assert_eq!(queue.pending(), 1, "take_ready 只搬队列，不计两次");
        assert_eq!(queue.bytes(), 300);

        queue.requeue_failed(taken);
        assert_eq!(
            queue.pending(),
            1,
            "requeue_failed 回灌同一批次后水位占用必须不变（当前实现重复计数）"
        );
        assert_eq!(queue.bytes(), 300, "字节水位同样不得翻倍");
    }
}
