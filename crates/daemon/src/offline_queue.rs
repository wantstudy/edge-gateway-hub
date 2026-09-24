//! 断网续传 —— SQLite 持久化队列（计划 task 17，Wave 3）。
//!
//! ## 职责边界
//! - **做**：采集数据的内存队列 + 磁盘兜底队列；背压（水位 / 硬上限 / 环形覆盖）；
//!   单调 `batch_seq` 幂等键；high-water mark（已确认位点）持久化与重启续传。
//! - **不做**：实际网络上传（task 19+ MQTT）、磁盘加密（task 18）、服务端侧幂等键校验（task 54）、
//!   遥测历史库 `telemetry.db` 的读写（task 18，本模块**物理隔离**、不共用库文件或写连接）。
//!
//! ## 关键设计决策
//!
//! 1. **独立库文件 + 单写者**。`queue.db` 与 task 18 的 `telemetry.db` 物理分离，各自持有独立
//!    写连接（SQLite 只允许单写者，共用一个连接会互相阻塞）。所有 SQL 写操作集中在**一个专用
//!    OS 写线程**（`std::thread` + `std::sync::mpsc`），对外只暴露 `&self` 接口。
//!    选型理由见「偏差说明 1」。
//! 2. **WAL + synchronous=NORMAL**。打开即 `PRAGMA journal_mode=WAL`，`synchronous=NORMAL`
//!    在 WAL 下已保证「提交后不丢事务」（仅崩溃时可能丢最后几笔），兼顾吞吐与持久性。
//! 3. **水位三级 + 溢出拒绝（task 54 修订）**：内存高水位（默认 8192 行，可配置，且
//!    不超过硬上限的 4/5）触发**降级**——新数据直接走离线落盘路径（不进内存、不阻塞
//!    生产者）；硬上限（默认 65536 行）触达后**拒绝入队**并返回溢出标记，同时记
//!    [`QueueAudit::Overflow`]——**绝不静默、绝不阻塞采集**（数据交还调用方，由其
//!    决定重试 / 告警，绝不在这里丢）。磁盘侧仍有保留期淘汰 + 环形覆盖
//!    （[`QueueAudit::Evicted`]）。
//! 4. **环形覆盖**：每个落盘事务末尾做一次维护——先按 `retention_ns`（注入时钟，测试零 sleep）
//!    淘汰过期批次，再按 `max_db_bytes` 从最旧开始逐条淘汰，均记 [`QueueAudit::Evicted`]。
//! 5. **幂等键 = (gateway_id, batch_seq)**。`batch_seq` 由可注入的
//!    [`BatchSeqSource`]（默认 [`AtomicSeqSource`]，原子单调递增）分配；分配结果在
//!    `queue_meta.last_seq` 持久化，保证进程重启后不回退、不复用。
//!    幂等键辅助见 [`QueuedBatch::idempotency_key`]；跨进程 JSON 传输时
//!    `batch_seq` **一律字符串**（大数红线，见 [`batch_meta_json`]）。
//! 6. **续传（Ack 先落后推）**：`ack_up_to(seq)` **先持久化确认记录**（默认写
//!    `queue_meta.ack_seq`，可注入 [`AckSink`]），持久化成功**后才**推进内存位点并删除
//!    `seq <= ack` 的行；`open` 时读回两者，重启后从 `ack_seq + 1` 继续补发——
//!    无重复、无空洞；「落 Ack 后崩溃」恢复位点，「崩溃在落 Ack 前」则靠幂等键重放去重。
//! 7. **顺序**：`take_batch` / `replay_batch` 合并磁盘与内存结果后按 `seq` 升序排序去重，
//!    同设备内严格有序；补发先读 high-water mark，跳过已确认位点之前的条目。
//! 8. **零 panic**：所有 rusqlite / io 错误收敛为 `DaemonError::StorageError`（4000），
//!    配置非法为 `ConfigError`（2000）；锁中毒时取回内部数据而非 unwrap。

use std::collections::VecDeque;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{Builder, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension, Transaction};

use crate::error::{DaemonError, DaemonResult};

// ---- 常量 ----

/// 队列库文件名（**固定**；与 task 18 的 `telemetry.db` 物理隔离）。
pub const QUEUE_DB_FILE_NAME: &str = "queue.db";

/// task 18 遥测库文件名（仅用于配置校验时的显式拦截，本模块不读写）。
pub const TELEMETRY_DB_FILE_NAME: &str = "telemetry.db";

/// 默认内存队列行数硬上限（task 54：65536 条，拒绝入队阈值）。
pub const DEFAULT_MAX_MEM_ROWS: usize = 65_536;

/// 默认内存队列行数高水位（task 54：8192 条；超过后新数据直接走落盘降级路径）。
///
/// 依据见 `docs/design/capacity-estimation.md`：2000 条/秒的上行下约 4 秒缓冲，
/// 既有降级余量又不至于让内存常驻过大。
pub const DEFAULT_MEM_HIGH_WATER_ROWS: usize = 8_192;

/// 默认内存队列字节硬上限（256MB）。
pub const DEFAULT_MAX_MEM_BYTES: usize = 256 * 1024 * 1024;

/// 默认磁盘队列字节上限（10GB，环形覆盖阈值）。
pub const DEFAULT_MAX_DB_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// 默认保留期（7 天，纳秒）。
pub const DEFAULT_RETENTION_NS: i64 = 7 * 24 * 60 * 60 * 1_000_000_000;

/// 审计环形缓冲上限（超出后淘汰最旧审计项，避免无界增长）。
const MAX_AUDIT_ENTRIES: usize = 10_000;

/// 高水位分子（4/5 = 80%）。
const HIGH_WATER_NUM: usize = 4;
/// 高水位分母。
const HIGH_WATER_DEN: usize = 5;

/// 写线程名（诊断用）。
const WRITER_THREAD_NAME: &str = "offline-queue-writer";

/// 写线程忙等超时（毫秒，避免瞬时锁冲突直接失败）。
const BUSY_TIMEOUT_MS: i64 = 3_000;

// ---- 时钟 ----

/// 可注入时钟（测试要模拟 7 天保留期，**禁止真 sleep**）。
pub trait Clock: Send + Sync {
    /// 当前时间（UNIX 纪元起纳秒）。
    fn now_ns(&self) -> i64;
}

/// 真实系统时钟。
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl SystemClock {
    /// 构造系统时钟。
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Clock for SystemClock {
    fn now_ns(&self) -> i64 {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
            // 系统时钟早于 UNIX 纪元（时钟异常）时退化为 0，绝不 panic。
            Err(_) => 0,
        }
    }
}

/// 手工时钟（测试用）：可 `set` / `advance`，共享同一份纳秒值。
#[derive(Debug, Clone)]
pub struct ManualClock {
    now: Arc<AtomicU64>,
}

impl ManualClock {
    /// 以指定起点（纳秒）构造。
    #[must_use]
    pub fn new(start_ns: i64) -> Self {
        Self {
            now: Arc::new(AtomicU64::new(start_ns.max(0) as u64)),
        }
    }

    /// 直接设置当前时间（纳秒）。
    pub fn set(&self, now_ns: i64) {
        self.now.store(now_ns.max(0) as u64, Ordering::SeqCst);
    }

    /// 在当前时间上累加 `delta_ns`（可为负，内部夹紧到 0）。
    pub fn advance(&self, delta_ns: i64) {
        if delta_ns >= 0 {
            self.now.fetch_add(delta_ns as u64, Ordering::SeqCst);
        } else {
            self.now
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |cur| {
                    Some(cur.saturating_sub(delta_ns.unsigned_abs()))
                })
                .ok();
        }
    }

    /// 读取当前时间（纳秒）。
    #[must_use]
    pub fn now(&self) -> i64 {
        self.now.load(Ordering::SeqCst) as i64
    }
}

impl Clock for ManualClock {
    fn now_ns(&self) -> i64 {
        self.now.load(Ordering::SeqCst) as i64
    }
}

// ---- 注入点：batch_seq 来源 / Ack 持久化（task 54） ----

/// `batch_seq` 来源（可注入，测试可控；task 54）。
///
/// 契约：`next_batch_seq` 必须**单调递增且进程内唯一**（幂等键
/// `gateway_id + batch_seq` 的正确性依赖于此）；默认实现 [`AtomicSeqSource`]
/// 以持久化的 `last_seq + 1` 为起点。注入自定义来源时由调用方负责单调性。
pub trait BatchSeqSource: Send + Sync {
    /// 分配下一个 `batch_seq`。
    fn next_batch_seq(&self) -> u64;

    /// 最近一次分配的 `batch_seq`（诊断用；从未分配过返回 0）。
    fn current_batch_seq(&self) -> u64;
}

/// 默认 `batch_seq` 来源：原子计数器（单调递增，线程安全）。
#[derive(Debug)]
pub struct AtomicSeqSource {
    /// 下一个待分配的 seq。
    next: AtomicU64,
}

impl AtomicSeqSource {
    /// 以 `start_seq` 为第一个分配值构造。
    #[must_use]
    pub fn new(start_seq: u64) -> Self {
        Self {
            next: AtomicU64::new(start_seq.max(1)),
        }
    }
}

impl BatchSeqSource for AtomicSeqSource {
    fn next_batch_seq(&self) -> u64 {
        // fetch_add 返回旧值：即本批 seq；并发下全局单调且唯一。
        self.next.fetch_add(1, Ordering::SeqCst)
    }

    fn current_batch_seq(&self) -> u64 {
        self.next.load(Ordering::SeqCst).saturating_sub(1)
    }
}

/// Ack 持久化（可注入；task 54「先落 Ack 后推位点」语义的持久化端点）。
///
/// 契约：`persist_ack` 成功返回后，该确认记录**必须已持久化**（进程崩溃后可恢复）；
/// 返回 `Err` 时调用方**不得**推进内存位点（数据继续保留、后续重放由幂等键去重）。
/// 默认（未注入）走内置 SQLite 写线程（`queue_meta.ack_seq` + 删除已确认行）。
pub trait AckSink: Send + Sync {
    /// 持久化一条确认记录（确认到 `seq` 为止）。
    ///
    /// # Errors
    /// 持久化失败返回 `StorageError`（4000）——内存位点保持不动。
    fn persist_ack(&self, seq: u64) -> DaemonResult<()>;
}

/// 队列装配钩子（`open_with_hooks` 注入；缺省字段走内置实现）。
#[derive(Default)]
pub struct QueueHooks {
    /// 自定义 `batch_seq` 来源（`None` = 内置 [`AtomicSeqSource`]，起点取持久化
    /// `last_seq + 1`）。注入来源时起点由调用方管理。
    pub seq_source: Option<Arc<dyn BatchSeqSource>>,
    /// 自定义 Ack 持久化（`None` = 内置 SQLite 写线程路径）。
    pub ack_sink: Option<Arc<dyn AckSink>>,
}

// ---- 配置 ----

/// 队列配置（非法值在 `open` 时转 `ConfigError`，错误码 2000）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueConfig {
    /// 队列库路径（文件名**必须**是 `queue.db`，禁止写成 `telemetry.db`）。
    pub db_path: PathBuf,
    /// 网关标识（幂等键组成之一，非空）。
    pub gateway_id: String,
    /// 内存队列行数硬上限（> 0；触达后拒绝入队并返回溢出标记，task 54）。
    pub max_mem_rows: usize,
    /// 内存队列行数高水位（> 0；超过后新数据直接走落盘降级路径，task 54）。
    ///
    /// 实际生效值会被夹紧到硬上限的 4/5 以内（见 [`Self::high_water_rows`]）。
    pub mem_high_water_rows: usize,
    /// 内存队列字节硬上限（> 0）。
    pub max_mem_bytes: usize,
    /// 磁盘队列字节上限（> 0，环形覆盖阈值）。
    pub max_db_bytes: u64,
    /// 磁盘保留期（纳秒，> 0）。
    pub retention_ns: i64,
}

impl QueueConfig {
    /// 按默认值构造并校验。
    ///
    /// # Errors
    /// 库文件名不是 `queue.db` / 网关标识为空时返回 `ConfigError`（2000）。
    pub fn new(db_path: impl Into<PathBuf>, gateway_id: impl Into<String>) -> DaemonResult<Self> {
        let config = Self {
            db_path: db_path.into(),
            gateway_id: gateway_id.into(),
            max_mem_rows: DEFAULT_MAX_MEM_ROWS,
            mem_high_water_rows: DEFAULT_MEM_HIGH_WATER_ROWS,
            max_mem_bytes: DEFAULT_MAX_MEM_BYTES,
            max_db_bytes: DEFAULT_MAX_DB_BYTES,
            retention_ns: DEFAULT_RETENTION_NS,
        };
        config.validate()?;
        Ok(config)
    }

    /// 校验全部字段；非法即 `ConfigError`（2000）。
    ///
    /// # Errors
    /// 任一字段非法时返回 `ConfigError`。
    pub fn validate(&self) -> DaemonResult<()> {
        let file_name = self
            .db_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if file_name.is_empty() {
            return Err(config_err("db_path must point to a database file"));
        }
        if file_name != QUEUE_DB_FILE_NAME {
            return Err(config_err(format!(
                "queue db file must be named `{QUEUE_DB_FILE_NAME}` (got `{file_name}`; \
                 `{TELEMETRY_DB_FILE_NAME}` belongs to task 18 and must not be shared)"
            )));
        }
        if self.gateway_id.trim().is_empty() {
            return Err(config_err("gateway_id must not be empty"));
        }
        if self.max_mem_rows == 0 {
            return Err(config_err("max_mem_rows must be greater than 0"));
        }
        if self.mem_high_water_rows == 0 {
            return Err(config_err("mem_high_water_rows must be greater than 0"));
        }
        if self.max_mem_bytes == 0 {
            return Err(config_err("max_mem_bytes must be greater than 0"));
        }
        if self.max_db_bytes == 0 {
            return Err(config_err("max_db_bytes must be greater than 0"));
        }
        if self.retention_ns <= 0 {
            return Err(config_err("retention_ns must be greater than 0"));
        }
        Ok(())
    }

    /// 生效的内存高水位行数（task 54）。
    ///
    /// 取 `min(配置值, 硬上限的 4/5)` 且至少 1：高水位必须严格低于硬上限，
    /// 否则降级路径没有存在的意义（直接顶到硬上限拒绝）。
    #[must_use]
    pub fn high_water_rows(&self) -> usize {
        let cap = (self.max_mem_rows.saturating_mul(HIGH_WATER_NUM) / HIGH_WATER_DEN).max(1);
        self.mem_high_water_rows.max(1).min(cap)
    }

    /// 高水位字节数（硬上限的 4/5，至少 1）。
    #[must_use]
    pub fn high_water_bytes(&self) -> usize {
        (self.max_mem_bytes.saturating_mul(HIGH_WATER_NUM) / HIGH_WATER_DEN).max(1)
    }
}

// ---- 数据模型 ----

/// 一个待上传批次（幂等键 = `gateway_id` + `seq`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedBatch {
    /// 单调递增批次序号（与 `gateway_id` 共同组成幂等键）。
    pub seq: u64,
    /// 网关标识。
    pub gateway_id: String,
    /// 批次负载（编码后的上报报文）。
    pub payload: Vec<u8>,
    /// 入队时刻（纳秒，用于保留期淘汰）。
    pub enqueued_ns: i64,
}

impl QueuedBatch {
    /// 负载字节数。
    #[must_use]
    pub fn payload_len(&self) -> usize {
        self.payload.len()
    }

    /// 幂等键（task 54）：`gateway_id + batch_seq`。
    ///
    /// 在补发 / 重放全过程中保持稳定（同一条目两次 `take_batch` 得到相同键），
    /// 接收端按此键去重即可保证断网重放不产生重复数据。
    #[must_use]
    pub fn idempotency_key(&self) -> String {
        idempotency_key(&self.gateway_id, self.seq)
    }
}

/// 幂等键生成（task 54）：`{gateway_id}:{batch_seq}`。
///
/// `:` 作为分隔符——`gateway_id` 若含 `:` 仍可由右侧最后一段还原 `batch_seq`，
/// 但建议网关标识避免使用该字符。
#[must_use]
pub fn idempotency_key(gateway_id: &str, batch_seq: u64) -> String {
    format!("{gateway_id}:{batch_seq}")
}

/// 审计事件：丢弃 / 淘汰 / 溢出 / 落盘**必须留痕**，绝不静默。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueAudit {
    /// 触达内存硬上限：拒绝入队并返回溢出标记（task 54）。
    ///
    /// 数据**不丢**——随 `enqueue` 的 `Err` 交还调用方，由其计数告警 / 重试；
    /// 本审计保证溢出「绝不静默」。
    Overflow {
        /// 被拒绝的批次序号（单调性保留，允许空洞）。
        seq: u64,
        /// 拒绝原因（可诊断）。
        reason: String,
    },
    /// 环形覆盖 / 保留期淘汰了一批磁盘批次。
    Evicted {
        /// 被淘汰的批次数。
        rows: usize,
        /// 被淘汰的字节数。
        bytes: u64,
        /// 淘汰原因（`retention_ns` 或 `max_db_bytes`）。
        reason: String,
    },
    /// 一次落盘事务写入的批次数（批量事务可观测性）。
    Flushed {
        /// 写入批次数。
        rows: usize,
    },
}

// ---- 内存态 ----

/// 内存队列（行数 + 字节数同锁保护，保证一致性）。
#[derive(Debug, Default)]
struct MemState {
    rows: VecDeque<QueuedBatch>,
    bytes: usize,
}

/// 磁盘统计（未 ack 的行数与逻辑字节数）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct DiskStats {
    rows: usize,
    bytes: u64,
}

/// 写线程启动后回传的位点状态。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct InitState {
    ack_seq: u64,
    last_seq: u64,
}

// ---- 写线程指令 ----

/// 单条指令的响应通道。
type Resp<T> = Sender<DaemonResult<T>>;

/// 写线程指令（全部经单一写连接串行执行）。
enum Cmd {
    /// 批量落盘（一个事务），返回写入行数。
    Persist {
        /// 待写入批次（按 seq 升序）。
        batches: Vec<QueuedBatch>,
        /// 响应通道。
        resp: Resp<usize>,
    },
    /// 取未 ack 批次（按 seq 升序，最多 `max` 条）。
    Take {
        /// 最多取多少条。
        max: usize,
        /// 只取 `seq > after_seq` 的行。
        after_seq: u64,
        /// 响应通道。
        resp: Resp<Vec<QueuedBatch>>,
    },
    /// 未 ack 行数与字节数。
    Stats {
        /// 只统计 `seq > after_seq` 的行。
        after_seq: u64,
        /// 响应通道。
        resp: Resp<DiskStats>,
    },
    /// 推进 high-water mark 并删除已确认行。
    Ack {
        /// 已确认到的最大 seq。
        seq: u64,
        /// 响应通道。
        resp: Resp<()>,
    },
    /// 停止写线程。
    Stop {
        /// 响应通道。
        resp: Resp<()>,
    },
}

// ---- 写线程 ----

/// 建表语句。
const SCHEMA_SQL: &str = r"
CREATE TABLE IF NOT EXISTS queue (
    seq         INTEGER PRIMARY KEY,
    gateway_id  TEXT    NOT NULL,
    payload     BLOB    NOT NULL,
    enqueued_ns INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS queue_meta (
    k TEXT PRIMARY KEY,
    v TEXT NOT NULL
);
";

/// 独占写连接的写线程主体（**唯一**的 SQLite 写者）。
struct DbWriter {
    conn: Connection,
    clock: Arc<dyn Clock>,
    max_db_bytes: u64,
    retention_ns: i64,
    audit: Arc<Mutex<VecDeque<QueueAudit>>>,
    fail_writes: Arc<AtomicBool>,
    ack_seq: u64,
    last_seq: u64,
}

impl DbWriter {
    /// 打开连接并施加 PRAGMA（WAL + synchronous=NORMAL + busy_timeout）。
    fn open_conn(path: &Path) -> DaemonResult<Connection> {
        let conn = Connection::open(path).map_err(map_sqlite)?;
        conn.execute_batch(&format!(
            "PRAGMA journal_mode = WAL;\nPRAGMA synchronous = NORMAL;\nPRAGMA busy_timeout = {BUSY_TIMEOUT_MS};"
        ))
        .map_err(map_sqlite)?;
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .map_err(map_sqlite)?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(storage_err(format!(
                "failed to enable WAL journal mode (got `{mode}`)"
            )));
        }
        // 迁移框架接线（task 55 → task 17 集成波次）：连接建立即执行内置迁移
        // （`schema_migrations` 账本 + `user_version` 版本登记，幂等）。
        // 分库红线不变：queue.db 仍走本模块**独立写连接**，与 telemetry.db 物理隔离；
        // 迁移失败（含「旧程序开新库」forward-only 拒绝）→ 错误上抛 → `open` 失败
        // → 启动失败，**绝不静默跳过**。
        crate::migrations::run_migrations(&conn)
            .map_err(|e| storage_err(format!("queue db migration failed: {e}")))?;
        Ok(conn)
    }

    /// 建表并读回持久化的位点。
    fn bootstrap(&mut self) -> DaemonResult<InitState> {
        self.conn.execute_batch(SCHEMA_SQL).map_err(map_sqlite)?;
        let max_row_seq: i64 = self
            .conn
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM queue", [], |row| {
                row.get(0)
            })
            .map_err(map_sqlite)?;
        let meta_last = self.read_meta_i64("last_seq")?.unwrap_or(0);
        let meta_ack = self.read_meta_i64("ack_seq")?.unwrap_or(0);
        self.last_seq = max_row_seq.max(meta_last).max(0) as u64;
        self.ack_seq = meta_ack.max(0) as u64;
        Ok(InitState {
            ack_seq: self.ack_seq,
            last_seq: self.last_seq,
        })
    }

    /// 指令循环（收到 `Stop` 或发送端全部释放即退出）。
    fn run(&mut self, rx: Receiver<Cmd>) {
        loop {
            match rx.recv() {
                Ok(Cmd::Stop { resp }) => {
                    let _ = resp.send(Ok(()));
                    break;
                }
                Ok(cmd) => self.handle(cmd),
                // 所有发送端已释放（队列被丢弃）：正常退出。
                Err(_) => break,
            }
        }
    }

    /// 处理单条指令（错误一律回传，绝不 panic、绝不退出循环）。
    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Persist { batches, resp } => {
                let result = self.persist(batches);
                let _ = resp.send(result);
            }
            Cmd::Take {
                max,
                after_seq,
                resp,
            } => {
                let result = self.take(max, after_seq);
                let _ = resp.send(result);
            }
            Cmd::Stats { after_seq, resp } => {
                let result = self.stats(after_seq);
                let _ = resp.send(result);
            }
            Cmd::Ack { seq, resp } => {
                let result = self.ack(seq);
                let _ = resp.send(result);
            }
            Cmd::Stop { resp } => {
                let _ = resp.send(Ok(()));
            }
        }
    }

    /// 批量落盘：一个事务写完所有批次，再跑一次维护（保留期 + 环形覆盖）。
    fn persist(&mut self, batches: Vec<QueuedBatch>) -> DaemonResult<usize> {
        if self.fail_writes.load(Ordering::SeqCst) {
            return Err(storage_err(
                "persist failed: disk backend is not writable (fault injection)",
            ));
        }
        let tx = self.conn.transaction().map_err(map_sqlite)?;
        {
            let mut stmt = tx
                .prepare(
                    "INSERT OR REPLACE INTO queue(seq, gateway_id, payload, enqueued_ns) \
                     VALUES(?1, ?2, ?3, ?4)",
                )
                .map_err(map_sqlite)?;
            for batch in &batches {
                stmt.execute(params![
                    seq_to_i64(batch.seq),
                    batch.gateway_id.as_str(),
                    batch.payload.as_slice(),
                    batch.enqueued_ns,
                ])
                .map_err(map_sqlite)?;
            }
        }
        // 维护（保留期淘汰 + 环形覆盖）在同一事务内完成，保证淘汰与写入原子生效。
        // 以静态函数形式调用：`maintain` 只读 `&self` 的配置与审计句柄，但此处
        // `self.conn` 已被 `tx` 可变借出（E0502），故把所需字段显式传参。
        maintain(
            &tx,
            self.clock.as_ref(),
            self.retention_ns,
            self.max_db_bytes,
            &self.audit,
        )?;
        for batch in &batches {
            self.last_seq = self.last_seq.max(batch.seq);
        }
        write_meta_i64(&tx, "last_seq", seq_to_i64(self.last_seq))?;
        tx.commit().map_err(map_sqlite)?;
        if !batches.is_empty() {
            self.push_audit(QueueAudit::Flushed {
                rows: batches.len(),
            });
        }
        Ok(batches.len())
    }

    /// 取未 ack 批次（严格按 seq 升序）。
    fn take(&mut self, max: usize, after_seq: u64) -> DaemonResult<Vec<QueuedBatch>> {
        if max == 0 {
            return Ok(Vec::new());
        }
        if self.fail_writes.load(Ordering::SeqCst) {
            // 读路径不受写故障注入影响（上传侧仍需能读到内存中的存量数据）。
            return Ok(Vec::new());
        }
        let limit = seq_to_i64(max.min(i64::MAX as usize) as u64);
        let mut stmt = self
            .conn
            .prepare(
                "SELECT seq, gateway_id, payload, enqueued_ns FROM queue \
                 WHERE seq > ?1 ORDER BY seq ASC LIMIT ?2",
            )
            .map_err(map_sqlite)?;
        let rows = stmt
            .query_map(params![seq_to_i64(after_seq), limit], |row| {
                Ok(QueuedBatch {
                    seq: i64_to_seq(row.get::<_, i64>(0)?),
                    gateway_id: row.get::<_, String>(1)?,
                    payload: row.get::<_, Vec<u8>>(2)?,
                    enqueued_ns: row.get::<_, i64>(3)?,
                })
            })
            .map_err(map_sqlite)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(map_sqlite)?);
        }
        Ok(out)
    }

    /// 未 ack 行数与逻辑字节数（字节数按 payload 长度统计，不含页开销）。
    fn stats(&mut self, after_seq: u64) -> DaemonResult<DiskStats> {
        let (rows, bytes): (i64, i64) = self
            .conn
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(LENGTH(payload)), 0) FROM queue WHERE seq > ?1",
                params![seq_to_i64(after_seq)],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .map_err(map_sqlite)?;
        Ok(DiskStats {
            rows: rows.max(0) as usize,
            bytes: bytes.max(0) as u64,
        })
    }

    /// 推进 high-water mark：持久化位点并删除已确认行（同一事务）。
    fn ack(&mut self, seq: u64) -> DaemonResult<()> {
        if seq <= self.ack_seq {
            return Ok(());
        }
        if self.fail_writes.load(Ordering::SeqCst) {
            return Err(storage_err(
                "ack failed: disk backend is not writable (fault injection)",
            ));
        }
        self.ack_seq = seq;
        let tx = self.conn.transaction().map_err(map_sqlite)?;
        tx.execute(
            "DELETE FROM queue WHERE seq <= ?1",
            params![seq_to_i64(seq)],
        )
        .map_err(map_sqlite)?;
        write_meta_i64(&tx, "ack_seq", seq_to_i64(seq))?;
        write_meta_i64(&tx, "last_seq", seq_to_i64(self.last_seq.max(seq)))?;
        tx.commit().map_err(map_sqlite)?;
        Ok(())
    }

    /// 维护：保留期淘汰 + 环形覆盖（均在调用方事务内）。
    /// 维护（保留期淘汰 + 环形覆盖）——委托给同名自由函数，仅作可读性入口。
    #[allow(dead_code)]
    fn maintain(&self, tx: &Transaction<'_>) -> DaemonResult<()> {
        maintain(
            tx,
            self.clock.as_ref(),
            self.retention_ns,
            self.max_db_bytes,
            &self.audit,
        )
    }
}

/// 维护（保留期淘汰 + 环形覆盖）的静态实现：只依赖显式传入的配置与审计句柄，
/// 不借用 `&self`，因此可在 `self.conn` 被事务可变借出时调用（规避 E0502）。
fn maintain(
    tx: &Transaction<'_>,
    clock: &dyn Clock,
    retention_ns: i64,
    max_db_bytes: u64,
    audit: &Arc<Mutex<VecDeque<QueueAudit>>>,
) -> DaemonResult<()> {
    // 1) 保留期：删除 enqueued_ns 早于 (now - retention_ns) 的批次。
    let cutoff = clock.now_ns().saturating_sub(retention_ns);
    let (old_rows, old_bytes): (i64, i64) = tx
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(LENGTH(payload)), 0) FROM queue WHERE enqueued_ns < ?1",
            params![cutoff],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .map_err(map_sqlite)?;
    if old_rows > 0 {
        tx.execute("DELETE FROM queue WHERE enqueued_ns < ?1", params![cutoff])
            .map_err(map_sqlite)?;
        push_audit(
            audit,
            QueueAudit::Evicted {
                rows: old_rows.max(0) as usize,
                bytes: old_bytes.max(0) as u64,
                reason: format!("retention_ns exceeded (cutoff {cutoff})"),
            },
        );
    }

    // 2) 环形覆盖：超过 max_db_bytes 时从最旧批次开始逐条淘汰。
    //    至少保留最新一条（单条负载就超过上限时不做无意义的全清）。
    let mut evicted_rows = 0usize;
    let mut evicted_bytes = 0u64;
    loop {
        let (total_rows, total_bytes): (i64, i64) = tx
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(LENGTH(payload)), 0) FROM queue",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .map_err(map_sqlite)?;
        if total_rows <= 1 || (total_bytes.max(0) as u64) <= max_db_bytes {
            break;
        }
        let (oldest_seq, payload_len): (i64, i64) = tx
            .query_row(
                "SELECT seq, LENGTH(payload) FROM queue ORDER BY seq ASC LIMIT 1",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .map_err(map_sqlite)?;
        tx.execute("DELETE FROM queue WHERE seq = ?1", params![oldest_seq])
            .map_err(map_sqlite)?;
        evicted_rows += 1;
        evicted_bytes += payload_len.max(0) as u64;
    }
    if evicted_rows > 0 {
        push_audit(
            audit,
            QueueAudit::Evicted {
                rows: evicted_rows,
                bytes: evicted_bytes,
                reason: format!("max_db_bytes exceeded (limit {max_db_bytes})"),
            },
        );
    }
    Ok(())
}

impl DbWriter {
    /// 读一个 meta 键（不存在返回 `None`）。
    fn read_meta_i64(&self, key: &str) -> DaemonResult<Option<i64>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT v FROM queue_meta WHERE k = ?1",
                params![key],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(map_sqlite)?;
        match raw {
            None => Ok(None),
            Some(text) => text
                .parse::<i64>()
                .map(Some)
                .map_err(|e| storage_err(format!("corrupt meta `{key}`: {e}"))),
        }
    }

    /// 追加审计（环形缓冲，超限淘汰最旧项）。
    fn push_audit(&self, event: QueueAudit) {
        push_audit(&self.audit, event);
    }
}

/// 写/读一个 meta 键（在给定事务内）。
fn write_meta_i64(tx: &Transaction<'_>, key: &str, value: i64) -> DaemonResult<()> {
    tx.execute(
        "INSERT INTO queue_meta(k, v) VALUES(?1, ?2) \
         ON CONFLICT(k) DO UPDATE SET v = excluded.v",
        params![key, value.to_string()],
    )
    .map_err(map_sqlite)?;
    Ok(())
}

// ---- 公开队列 ----

/// 断网续传队列：内存队列（背压）+ SQLite 磁盘队列（环形覆盖）+ 单写线程。
pub struct OfflineQueue {
    cfg: QueueConfig,
    clock: Arc<dyn Clock>,
    mem: Mutex<MemState>,
    /// `batch_seq` 来源（可注入；默认起点 = 持久化 last_seq + 1）。
    seq_source: Arc<dyn BatchSeqSource>,
    /// Ack 持久化端点（可注入；`None` 表示走内置 SQLite 写线程）。
    ack_sink: Option<Arc<dyn AckSink>>,
    ack_seq: AtomicU64,
    online: AtomicBool,
    fail_writes: Arc<AtomicBool>,
    tx: Sender<Cmd>,
    audit: Arc<Mutex<VecDeque<QueueAudit>>>,
    handle: Mutex<Option<JoinHandle<()>>>,
    closed: AtomicBool,
}

impl std::fmt::Debug for OfflineQueue {
    /// 手写 Debug（`Mutex`/通道不实现 `Debug`）；**不打印 payload**，只给可观测量。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OfflineQueue")
            .field("gateway_id", &self.cfg.gateway_id)
            .field("db_path", &self.cfg.db_path)
            .field("ack_seq", &self.ack_seq.load(Ordering::SeqCst))
            .field("last_batch_seq", &self.seq_source.current_batch_seq())
            .field("online", &self.online.load(Ordering::SeqCst))
            .field("pending_mem", &self.mem().rows.len())
            .field("closed", &self.closed.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl OfflineQueue {
    /// 打开（或创建）队列库并启动单写线程（内置 seq 来源与 Ack 持久化）。
    ///
    /// # Errors
    /// - 配置非法 → `ConfigError`（2000）；
    /// - 目录创建 / SQLite 打开 / WAL 启用 / 建表失败 → `StorageError`（4000）。
    pub fn open(cfg: QueueConfig, clock: Arc<dyn Clock>) -> DaemonResult<Self> {
        Self::open_with_hooks(cfg, clock, QueueHooks::default())
    }

    /// 打开队列并注入装配钩子（task 54：自定义 `batch_seq` 来源 / Ack 持久化）。
    ///
    /// # Errors
    /// 同 [`Self::open`]。
    pub fn open_with_hooks(
        cfg: QueueConfig,
        clock: Arc<dyn Clock>,
        hooks: QueueHooks,
    ) -> DaemonResult<Self> {
        cfg.validate()?;

        if let Some(parent) = cfg.db_path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| storage_err(format!("create db dir {}: {e}", parent.display())))?;
            }
        }

        let (cmd_tx, cmd_rx) = channel::<Cmd>();
        let (init_tx, init_rx) = channel::<DaemonResult<InitState>>();
        let audit: Arc<Mutex<VecDeque<QueueAudit>>> = Arc::new(Mutex::new(VecDeque::new()));
        let fail_writes = Arc::new(AtomicBool::new(false));

        let db_path = cfg.db_path.clone();
        let cfg_for_writer = cfg.clone();
        let clock_for_writer = Arc::clone(&clock);
        let audit_for_writer = Arc::clone(&audit);
        let fail_for_writer = Arc::clone(&fail_writes);
        let handle = Builder::new()
            .name(WRITER_THREAD_NAME.to_string())
            .spawn(move || {
                let boot = DbWriter::open_conn(&db_path).and_then(|conn| {
                    let mut writer = DbWriter {
                        conn,
                        clock: clock_for_writer,
                        max_db_bytes: cfg_for_writer.max_db_bytes,
                        retention_ns: cfg_for_writer.retention_ns,
                        audit: audit_for_writer,
                        fail_writes: fail_for_writer,
                        ack_seq: 0,
                        last_seq: 0,
                    };
                    let state = writer.bootstrap()?;
                    Ok((writer, state))
                });
                match boot {
                    Ok((mut writer, state)) => {
                        let _ = init_tx.send(Ok(state));
                        // 初始化成功后进入指令循环，直到收到 Stop 或发送端全部释放。
                        writer.run(cmd_rx);
                    }
                    Err(err) => {
                        let _ = init_tx.send(Err(err));
                    }
                }
            })
            .map_err(|e| storage_err(format!("spawn queue writer thread: {e}")))?;

        let init = init_rx
            .recv()
            .map_err(|_| storage_err("queue writer thread died during init"))??;

        // batch_seq 来源：未注入则用内置原子计数器（起点 = 持久化 last_seq + 1，
        // 保证重启后不回退、不复用）。
        let seq_source: Arc<dyn BatchSeqSource> = hooks
            .seq_source
            .unwrap_or_else(|| Arc::new(AtomicSeqSource::new(init.last_seq.saturating_add(1))));

        Ok(Self {
            cfg,
            clock,
            mem: Mutex::new(MemState::default()),
            seq_source,
            ack_sink: hooks.ack_sink,
            ack_seq: AtomicU64::new(init.ack_seq),
            online: AtomicBool::new(true),
            fail_writes,
            tx: cmd_tx,
            audit,
            handle: Mutex::new(Some(handle)),
            closed: AtomicBool::new(false),
        })
    }

    /// 入队：返回分配的 `batch_seq`（单调递增，构成幂等键）。
    ///
    /// 背压水位策略（task 54，**绝不阻塞生产者、绝不静默丢数据**）：
    /// - 离线：直接落盘（保证断电也不丢），落盘失败即返回 `StorageError`；
    /// - 在线且内存 < 高水位：进内存；推入后达高水位 → 整队列落盘（失败则留内存）；
    /// - 在线且内存 ≥ 高水位：**降级**——本批直接走离线落盘路径（不进内存）；
    /// - 触达硬上限：**拒绝入队**并返回溢出标记（`Err`），同时记
    ///   [`QueueAudit::Overflow`]——数据交还调用方计数告警，绝不在这里丢弃。
    ///
    /// # Errors
    /// - 队列已关闭 → `StorageError`（4000）；
    /// - 离线且落盘失败 → `StorageError`（4000）；
    /// - 在线且内存触硬上限 → `StorageError`（4000，含溢出标记语义，见审计）。
    pub fn enqueue(&self, payload: Vec<u8>) -> DaemonResult<u64> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(storage_err("enqueue rejected: queue is closed"));
        }
        // seq 来源可注入（测试可控）；默认原子计数器，起点 = 持久化 last_seq + 1。
        let seq = self.seq_source.next_batch_seq();
        let batch = QueuedBatch {
            seq,
            gateway_id: self.cfg.gateway_id.clone(),
            payload,
            enqueued_ns: self.clock.now_ns(),
        };

        if !self.online.load(Ordering::SeqCst) {
            // 离线：直接落盘（磁盘有自己的环形覆盖 + Evicted 审计兜底）。
            self.send_persist(vec![batch])?;
            return Ok(seq);
        }

        // 容量判定在锁内只做**读判定**（绝不持锁做 IO → 慢盘不阻塞采集线程）。
        let over_high_water = {
            let mem = self.mem();
            if mem.rows.len() >= self.cfg.max_mem_rows || mem.bytes >= self.cfg.max_mem_bytes {
                // 硬上限：拒绝入队 + 溢出审计（绝不静默；数据交还调用方）。
                push_audit(
                    &self.audit,
                    QueueAudit::Overflow {
                        seq,
                        reason: format!(
                            "memory hard limit reached (rows {}/{}, bytes {}/{}); \
                             enqueue rejected, data returned to caller",
                            mem.rows.len(),
                            self.cfg.max_mem_rows,
                            mem.bytes,
                            self.cfg.max_mem_bytes
                        ),
                    },
                );
                return Err(storage_err(format!(
                    "enqueue rejected: memory queue at hard limit \
                     (batch_seq {seq} overflowed, data returned to caller)"
                )));
            }
            mem.rows.len() >= self.cfg.high_water_rows() || mem.bytes >= self.cfg.high_water_bytes()
        };

        // 超高水位 → 降级：新数据直接走离线落盘路径（不进内存）。
        // （负载克隆仅发生在降级分支；常规路径零克隆。失败则继续收内存保数据。）
        let payload_len = batch.payload_len();
        if over_high_water && self.send_persist(vec![batch.clone()]).is_ok() {
            return Ok(seq);
        }
        // 未超高水位，或降级落盘不可用（此时内存必然仍有硬上限余量，见上方守卫）：
        // 收进内存保数据。

        let now_over_high_water = {
            let mut mem = self.mem();
            mem.rows.push_back(batch);
            mem.bytes = mem.bytes.saturating_add(payload_len);
            mem.rows.len() >= self.cfg.high_water_rows() || mem.bytes >= self.cfg.high_water_bytes()
        };
        // 达高水位 → 整队列落盘；失败则数据留内存（后续入队走硬上限拒绝路径）。
        if now_over_high_water {
            let _ = self.flush();
        }
        Ok(seq)
    }

    /// 未 ack 批次数（内存 + 磁盘）。
    ///
    /// # Errors
    /// 写线程不可用时返回 `StorageError`（4000）。
    pub fn pending(&self) -> DaemonResult<usize> {
        let disk = self.stats()?.rows;
        Ok(disk.saturating_add(self.pending_mem()))
    }

    /// 内存中未 ack 批次数。
    #[must_use]
    pub fn pending_mem(&self) -> usize {
        self.mem().rows.len()
    }

    /// 磁盘中未 ack 批次数。
    ///
    /// # Errors
    /// 写线程不可用时返回 `StorageError`（4000）。
    pub fn pending_disk(&self) -> DaemonResult<usize> {
        Ok(self.stats()?.rows)
    }

    /// 磁盘中未 ack 批次的逻辑字节数（`SUM(LENGTH(payload))`）。
    ///
    /// # Errors
    /// 写线程不可用时返回 `StorageError`（4000）。
    pub fn disk_bytes(&self) -> DaemonResult<u64> {
        Ok(self.stats()?.bytes)
    }

    /// 取最多 `max` 条未 ack 批次，**严格按 `seq` 升序**（供上传侧按序补发）。
    ///
    /// 取走不会删除；确认上传成功后调用 [`Self::ack_up_to`] 推进位点。
    ///
    /// # Errors
    /// 写线程不可用时返回 `StorageError`（4000）。
    pub fn take_batch(&self, max: usize) -> DaemonResult<Vec<QueuedBatch>> {
        if max == 0 {
            return Ok(Vec::new());
        }
        let after_seq = self.ack_seq.load(Ordering::SeqCst);
        let mut merged = self.send_cmd(|resp| Cmd::Take {
            max,
            after_seq,
            resp,
        })?;
        let mem = {
            let mem = self.mem();
            mem.rows
                .iter()
                .filter(|b| b.seq > after_seq)
                .cloned()
                .collect::<Vec<_>>()
        };
        merged.extend(mem);
        merged.sort_by_key(|b| b.seq);
        merged.dedup_by_key(|b| b.seq);
        merged.truncate(max);
        Ok(merged)
    }

    /// 补发批次（task 54）：先读 high-water mark，返回 `seq > ack_seq` 的未确认
    /// 批次（严格升序，最多 `max` 条）。
    ///
    /// 与 [`Self::take_batch`] 同一实现（补发语义命名）：重复调用（模拟重启后重放）
    /// 会再次返回同一批条目——其幂等键 `gateway_id + batch_seq` 保持稳定，接收端
    /// 按键去重即可保证重放不产生重复数据。确认后调用 [`Self::ack_up_to`] 推进位点。
    ///
    /// # Errors
    /// 写线程不可用时返回 `StorageError`（4000）。
    pub fn replay_batch(&self, max: usize) -> DaemonResult<Vec<QueuedBatch>> {
        self.take_batch(max)
    }

    /// 推进 high-water mark（task 54 Ack 语义：**先落 Ack，后推位点**）。
    ///
    /// 第一步：持久化确认记录（默认写 `queue_meta.ack_seq` 并删除已确认行；可注入
    /// [`AckSink`]）。持久化失败 → 内存位点**不推进**、数据不删，避免丢数据。
    /// 第二步：持久化成功后才推进内存位点并移除内存中已确认批次。
    ///
    /// 位点只增不减；重启后从 `ack_seq + 1` 续传 —— 无重复、无空洞；「崩溃在落 Ack
    /// 前」则该批会在重启后被重放，由幂等键 `gateway_id + batch_seq` 在接收端去重。
    ///
    /// # Errors
    /// 位点持久化失败 → `StorageError`（4000）；失败时内存位点**不推进**。
    pub fn ack_up_to(&self, seq: u64) -> DaemonResult<()> {
        let current = self.ack_seq.load(Ordering::SeqCst);
        if seq <= current {
            return Ok(());
        }
        // 第一步（先落 Ack）：持久化确认记录。
        match &self.ack_sink {
            Some(sink) => sink.persist_ack(seq)?,
            None => self.send_cmd(|resp| Cmd::Ack { seq, resp })?,
        }
        // 第二步（后推位点）：持久化成功后才动内存。
        self.ack_seq.store(seq, Ordering::SeqCst);
        // 内存侧同步移除已确认批次。
        let mut mem = self.mem();
        while mem.rows.front().is_some_and(|b| b.seq <= seq) {
            if let Some(front) = mem.rows.pop_front() {
                mem.bytes = mem.bytes.saturating_sub(front.payload_len());
            }
        }
        Ok(())
    }

    /// 当前 high-water mark（已确认到的最大 `batch_seq`）。
    #[must_use]
    pub fn ack_seq(&self) -> u64 {
        self.ack_seq.load(Ordering::SeqCst)
    }

    /// 强制把内存队列全部落盘（**一个事务**），返回写入行数。
    ///
    /// 即使内存为空也会跑一次维护（保留期 / 环形覆盖）。落盘失败时数据**回灌内存**，绝不静默丢弃。
    ///
    /// # Errors
    /// 写线程不可用 / SQLite 失败 → `StorageError`（4000）。
    pub fn flush(&self) -> DaemonResult<usize> {
        let batches: Vec<QueuedBatch> = {
            let mut mem = self.mem();
            let taken: Vec<QueuedBatch> = mem.rows.drain(..).collect();
            mem.bytes = 0;
            taken
        };
        match self.send_persist(batches.clone()) {
            Ok(rows) => Ok(rows),
            Err(err) => {
                // 落盘失败：数据回灌到内存队首（顺序不变），交由水位策略处理。
                self.restore_to_mem_front(batches);
                Err(err)
            }
        }
    }

    /// 取走并清空审计记录（测试断言用）。
    #[must_use]
    pub fn drain_audit(&self) -> Vec<QueueAudit> {
        let mut audit = match self.audit.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        audit.drain(..).collect()
    }

    /// 断网 / 恢复开关：`false` 时 `enqueue` 直接落盘（绕过内存队列）。
    pub fn set_online(&self, online: bool) {
        self.online.store(online, Ordering::SeqCst);
    }

    /// 当前是否在线。
    #[must_use]
    pub fn is_online(&self) -> bool {
        self.online.load(Ordering::SeqCst)
    }

    /// 故障注入开关（默认 `false`）：置 `true` 后所有磁盘写入返回 `StorageError`，
    /// 用于验证「落盘不可写 → 高水位降级失败 → 硬上限拒绝入队并留溢出审计」
    /// 的背压路径（task 54；QA Error 场景）。
    pub fn set_persist_failure(&self, fail: bool) {
        self.fail_writes.store(fail, Ordering::SeqCst);
    }

    /// 队列库路径（`queue.db`）。
    #[must_use]
    pub fn queue_db_path(&self) -> PathBuf {
        self.cfg.db_path.clone()
    }

    /// 网关标识（幂等键组成之一）。
    #[must_use]
    pub fn gateway_id(&self) -> &str {
        self.cfg.gateway_id.as_str()
    }

    /// 队列配置（只读）。
    #[must_use]
    pub fn config(&self) -> &QueueConfig {
        &self.cfg
    }

    /// 优雅关闭：先落盘，再停止写线程并 join。
    ///
    /// # Errors
    /// 最后一次落盘失败 → `StorageError`（4000）；写线程仍会被停止。
    pub fn close(&self) -> DaemonResult<()> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let flush_result = self.flush();
        let _ = self.send_cmd(|resp| Cmd::Stop { resp });
        if let Some(handle) = self.handle.lock().unwrap_or_else(poison_recover).take() {
            let _ = handle.join();
        }
        flush_result.map(|_rows| ())
    }

    // ---- 内部辅助 ----

    /// 取内存态锁（锁中毒时取回数据，绝不 panic）。
    fn mem(&self) -> MutexGuard<'_, MemState> {
        self.mem.lock().unwrap_or_else(poison_recover)
    }

    /// 落盘失败后把批次按原顺序回灌到内存队首。
    fn restore_to_mem_front(&self, batches: Vec<QueuedBatch>) {
        let mut mem = self.mem();
        let mut restored_bytes = 0usize;
        for batch in batches.into_iter().rev() {
            restored_bytes = restored_bytes.saturating_add(batch.payload_len());
            mem.rows.push_front(batch);
        }
        mem.bytes = mem.bytes.saturating_add(restored_bytes);
    }

    /// 发送指令并等待响应。
    fn send_cmd<T>(&self, make: impl FnOnce(Resp<T>) -> Cmd) -> DaemonResult<T> {
        let (resp_tx, resp_rx) = channel::<DaemonResult<T>>();
        let cmd = make(resp_tx);
        self.tx
            .send(cmd)
            .map_err(|_| storage_err("queue writer thread is gone"))?;
        match resp_rx.recv() {
            Ok(result) => result,
            Err(_) => Err(storage_err("queue writer thread did not respond")),
        }
    }

    /// 落盘指令。
    fn send_persist(&self, batches: Vec<QueuedBatch>) -> DaemonResult<usize> {
        self.send_cmd(|resp| Cmd::Persist { batches, resp })
    }

    /// 磁盘统计指令。
    fn stats(&self) -> DaemonResult<DiskStats> {
        let after_seq = self.ack_seq.load(Ordering::SeqCst);
        self.send_cmd(|resp| Cmd::Stats { after_seq, resp })
    }
}

impl Drop for OfflineQueue {
    fn drop(&mut self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        // 进程退出前尽最大努力把内存数据落盘（best effort，不返回错误、不 panic）。
        let _ = self.flush();
        let _ = self.send_cmd(|resp| Cmd::Stop { resp });
        if let Ok(mut guard) = self.handle.lock() {
            if let Some(handle) = guard.take() {
                let _ = handle.join();
            }
        }
    }
}

// ---- 工具函数 ----

/// `u64` seq → SQLite `INTEGER`（i64 域内位模式一致，取回时转回 `u64`）。
fn seq_to_i64(seq: u64) -> i64 {
    seq as i64
}

/// 批次元信息 JSON（task 54，供北向 JSON 出口 / 落盘信封复用）。
///
/// **大数红线**：`batch_seq` / `payload_bytes` 一律编码为**字符串**——
/// JavaScript `Number` 只能安全表示 2^53 以内的整数，`u64` 序号超过后若按
/// 数值编码会静默丢失精度，破坏幂等键去重。
#[must_use]
pub fn batch_meta_json(batch: &QueuedBatch) -> String {
    format!(
        "{{\"gateway_id\":\"{}\",\"batch_seq\":\"{}\",\"payload_bytes\":\"{}\"}}",
        json_escape(&batch.gateway_id),
        batch.seq,
        batch.payload.len()
    )
}

/// JSON 字符串转义（最少实现：反斜杠与双引号；控制字符按 JSON 规范转义）。
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

/// SQLite `INTEGER` → `u64` seq。
fn i64_to_seq(value: i64) -> u64 {
    value as u64
}

/// rusqlite 错误 → `StorageError`（4000）。
fn map_sqlite(err: rusqlite::Error) -> DaemonError {
    DaemonError::StorageError(format!("sqlite: {err}"))
}

/// 构造 `StorageError`（4000）。
fn storage_err(msg: impl fmt::Display) -> DaemonError {
    DaemonError::StorageError(msg.to_string())
}

/// 构造 `ConfigError`（2000）。
fn config_err(msg: impl fmt::Display) -> DaemonError {
    DaemonError::ConfigError(msg.to_string())
}

/// 锁中毒恢复（取回内部数据，绝不 panic）。
fn poison_recover<T>(poisoned: std::sync::PoisonError<T>) -> T {
    poisoned.into_inner()
}

/// 追加审计到环形缓冲（超限淘汰最旧项）。
fn push_audit(audit: &Arc<Mutex<VecDeque<QueueAudit>>>, event: QueueAudit) {
    let mut guard = match audit.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.push_back(event);
    while guard.len() > MAX_AUDIT_ENTRIES {
        guard.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{ERR_CONFIG, ERR_STORAGE};
    use rusqlite::OpenFlags;
    use std::collections::HashSet;
    use std::fs;
    use std::thread;
    use tempfile::TempDir;

    /// 测试用一天（纳秒）。
    const DAY_NS: i64 = 24 * 60 * 60 * 1_000_000_000;

    /// 测试起点（2024-01-01 前后，任意固定值即可）。
    const T0_NS: i64 = 1_700_000_000_000_000_000;

    /// 构造临时目录。
    fn tempdir() -> TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    /// 构造默认配置（`dir/queue.db`）。
    fn cfg(dir: &Path, gateway: &str) -> QueueConfig {
        QueueConfig::new(dir.join(QUEUE_DB_FILE_NAME), gateway).expect("config")
    }

    /// 打开队列（手工时钟）。
    fn open(dir: &Path, gateway: &str, clock: &ManualClock) -> OfflineQueue {
        OfflineQueue::open(cfg(dir, gateway), Arc::new(clock.clone())).expect("open queue")
    }

    // ---- 迁移框架接线（task 55 集成波次） ----

    /// QA 接线：`open` 成功的库必须已走迁移框架 —— `user_version = 1`、
    /// `schema_migrations` 账本恰 1 行（内置 v1）。分库红线：queue.db 仍是
    /// 独立库文件 + 独立写连接。
    #[test]
    fn open_runs_builtin_migration_and_ledger() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let queue = open(dir.path(), "gw-mig", &clock);
        let db_path = queue.queue_db_path();
        queue.close().expect("close");

        let ro = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("open read-only");
        let version: i64 = ro
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .expect("user_version");
        assert_eq!(version, 1, "内置 v1 迁移必须已应用");
        let ledger_rows: i64 = ro
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .expect("ledger count");
        assert_eq!(ledger_rows, 1, "账本恰 1 行（v1）");
    }

    /// QA 接线 Error：库文件损坏（非 SQLite 格式）→ 连接初始化 / 迁移失败 →
    /// `open` 失败（启动失败、可解释错误，绝不静默跳过）。
    #[test]
    fn open_fails_when_migration_fails_on_corrupt_db() {
        let dir = tempdir();
        let db_path = dir.path().join(QUEUE_DB_FILE_NAME);
        std::fs::write(&db_path, b"garbage bytes, not a sqlite database").expect("seed garbage");

        let clock = ManualClock::new(T0_NS);
        let err = OfflineQueue::open(cfg(dir.path(), "gw-corrupt"), Arc::new(clock))
            .expect_err("corrupt db must fail during open init");
        assert_eq!(err.error_code(), ERR_STORAGE);
        assert!(
            err.to_string().contains("sqlite:"),
            "错误须来自 SQLite 初始化链路: {err}"
        );
    }

    /// QA Happy：断网写入 100 条 → 恢复 → 全部按序补发且无重复。
    #[test]
    fn qa_happy_offline_100_rows_replayed_in_order_without_duplicates() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let queue = open(dir.path(), "gw-happy", &clock);
        queue.set_online(false);

        let mut expected: Vec<(u64, Vec<u8>)> = Vec::new();
        for i in 0..100u32 {
            let payload = format!("batch-{i:03}").into_bytes();
            let seq = queue.enqueue(payload.clone()).expect("enqueue offline");
            expected.push((seq, payload));
        }
        assert_eq!(queue.pending().expect("pending"), 100, "断网写入不得丢数据");
        assert_eq!(queue.pending_disk().expect("disk"), 100, "断网应直接落盘");
        assert_eq!(queue.pending_mem(), 0);

        queue.set_online(true);
        let batch = queue.take_batch(100).expect("take_batch");
        assert_eq!(batch.len(), 100, "补发必须完整");

        let mut prev = 0u64;
        let mut seen: HashSet<u64> = HashSet::new();
        for (i, got) in batch.iter().enumerate() {
            assert!(got.seq > prev, "seq 必须严格递增: {prev} -> {}", got.seq);
            prev = got.seq;
            assert_eq!(got.seq, expected[i].0);
            assert_eq!(got.payload, expected[i].1, "payload 必须与写入顺序一致");
            assert_eq!(got.gateway_id, "gw-happy");
            assert!(seen.insert(got.seq), "重复 seq: {}", got.seq);
        }
        assert_eq!(seen.len(), 100, "不得出现重复");
        assert_eq!(batch.first().map(|b| b.seq), Some(1));
        assert_eq!(batch.last().map(|b| b.seq), Some(100));
        queue.close().expect("close");
    }

    /// QA Error：补发中途进程重启 → 按 high-water mark 续传，无重复无空洞。
    #[test]
    fn restart_after_partial_ack_resumes_from_high_water_mark() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let queue = open(dir.path(), "gw-resume", &clock);
        queue.set_online(true);
        for i in 0..50u32 {
            queue
                .enqueue(format!("row-{i:02}").into_bytes())
                .expect("enqueue");
        }
        queue.ack_up_to(20).expect("ack_up_to");
        assert_eq!(queue.ack_seq(), 20);
        assert_eq!(queue.pending().expect("pending"), 30);
        queue.close().expect("close");

        // 模拟进程重启：同一 db_path 重新 open。
        let reopened = open(dir.path(), "gw-resume", &clock);
        assert_eq!(reopened.ack_seq(), 20, "位点必须持久化");
        assert_eq!(
            reopened.pending().expect("pending"),
            30,
            "重启后未 ack 数不变"
        );

        let batch = reopened.take_batch(50).expect("take_batch");
        assert_eq!(batch.len(), 30);
        assert_eq!(batch.first().map(|b| b.seq), Some(21), "必须从 ack+1 续传");
        assert_eq!(batch.last().map(|b| b.seq), Some(50));
        for (i, got) in batch.iter().enumerate() {
            assert_eq!(got.seq, 21 + i as u64, "不得有空洞: {:?}", got.seq);
            assert_eq!(got.payload, format!("row-{:02}", 20 + i).into_bytes());
        }
        // seq 不得回退复用。
        assert_eq!(
            reopened
                .enqueue(b"after-restart".to_vec())
                .expect("enqueue"),
            51
        );
        reopened.close().expect("close");
    }

    /// 高水位触发落盘（内存清空、磁盘有数据、总数不丢）。
    #[test]
    fn high_water_mark_forces_flush_to_disk() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let mut config = cfg(dir.path(), "gw-high-water");
        config.max_mem_rows = 10; // 高水位 = 8
        let queue = OfflineQueue::open(config, Arc::new(clock.clone())).expect("open");
        assert_eq!(queue.config().high_water_rows(), 8);

        for i in 0..25u32 {
            queue
                .enqueue(format!("row-{i:02}").into_bytes())
                .expect("enqueue");
        }
        assert_eq!(queue.pending_mem(), 1, "高水位触发后内存应被清空到 1 条");
        assert_eq!(queue.pending_disk().expect("disk"), 24, "其余应已落盘");
        assert_eq!(queue.pending().expect("pending"), 25, "总数不得丢失");

        let batch = queue.take_batch(100).expect("take_batch");
        assert_eq!(batch.len(), 25);
        for (i, got) in batch.iter().enumerate() {
            assert_eq!(got.seq, i as u64 + 1);
            assert_eq!(got.payload, format!("row-{i:02}").into_bytes());
        }
        queue.close().expect("close");
    }

    /// QA Error（task 54）：硬上限 → **拒绝入队** + 溢出审计留痕，且已收数据零丢失。
    ///
    /// 场景：max_mem_rows=5（高水位=4）+ 落盘故障注入 → 高水位降级也失败，
    /// 数据只能收内存；第 6 条触硬上限被拒（Overflow 审计），前 5 条原样可补发。
    #[test]
    fn hard_limit_rejects_enqueue_with_overflow_audit_and_no_data_loss() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let mut config = cfg(dir.path(), "gw-hard-limit");
        config.max_mem_rows = 5;
        let queue = OfflineQueue::open(config, Arc::new(clock.clone())).expect("open");
        assert_eq!(queue.config().high_water_rows(), 4);
        // 模拟「落盘不可写」：高水位降级路径也必然失败，数据只能收内存。
        queue.set_persist_failure(true);

        let mut accepted = Vec::new();
        for i in 0..6u32 {
            match queue.enqueue(format!("p-{i:02}").into_bytes()) {
                Ok(seq) => accepted.push(seq),
                Err(err) => {
                    // 第 6 条必须被拒：溢出标记 + StorageError(4000)。
                    assert_eq!(u64::from(i), 5, "只有第 6 条被拒，实际第 {} 条", i + 1);
                    assert_eq!(err.error_code(), ERR_STORAGE, "err: {err}");
                    assert!(
                        err.to_string().contains("overflowed"),
                        "错误信息必须携带溢出标记: {err}"
                    );
                }
            }
        }
        assert_eq!(accepted, vec![1, 2, 3, 4, 5], "前 5 条正常接收");
        assert_eq!(queue.pending_mem(), 5, "被拒的数据不得挤占内存");
        assert_eq!(queue.pending().expect("pending"), 5, "总数 = 已收 5 条");

        // 溢出必须留审计（绝不静默）：Overflow{seq: 6, reason 含 hard limit}。
        let audit = queue.drain_audit();
        assert!(
            audit.iter().any(|event| matches!(
                event,
                QueueAudit::Overflow { seq: 6, reason } if reason.contains("hard limit")
            )),
            "必须产生溢出审计: {audit:?}"
        );

        // 已收数据零丢失、零重复：幂等键完整可补发。
        let batch = queue.take_batch(10).expect("take_batch");
        let keys: Vec<String> = batch.iter().map(|b| b.idempotency_key()).collect();
        assert_eq!(keys.len(), 5);
        {
            let mut seen = std::collections::HashSet::new();
            for key in &keys {
                assert!(seen.insert(key.clone()), "幂等键重复: {key}");
            }
        }
        assert_eq!(
            batch.last().map(|b| b.payload.clone()),
            Some(b"p-04".to_vec())
        );
        queue.set_persist_failure(false);
        queue.close().expect("close");
    }

    /// QA Error：7 天保留期到期 → 旧批次被淘汰并产生 Evicted 审计（零真实 sleep）。
    #[test]
    fn retention_eviction_after_clock_advances_past_seven_days() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let queue = open(dir.path(), "gw-retention", &clock);
        queue.set_online(false);
        for i in 0..5u32 {
            queue
                .enqueue(format!("old-{i}").into_bytes())
                .expect("enqueue");
        }
        assert_eq!(queue.pending_disk().expect("disk"), 5);

        // 推进 8 天（注入时钟，不 sleep），再触发一次维护。
        clock.advance(8 * DAY_NS);
        queue.flush().expect("flush");

        assert_eq!(queue.pending_disk().expect("disk"), 0, "超期批次应被淘汰");
        let audit = queue.drain_audit();
        assert!(
            audit.iter().any(|event| matches!(
                event,
                QueueAudit::Evicted { rows: 5, reason, .. } if reason.contains("retention_ns")
            )),
            "必须产生 Evicted 审计: {audit:?}"
        );
        queue.close().expect("close");
    }

    /// QA Error：磁盘字节超 10GB 阈值（此处压到 4KB）→ 环形覆盖淘汰最旧。
    #[test]
    fn ring_overwrite_when_db_bytes_exceed_limit() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let mut config = cfg(dir.path(), "gw-ring");
        config.max_db_bytes = 4 * 1024; // 4KB
        let queue = OfflineQueue::open(config, Arc::new(clock.clone())).expect("open");

        for _ in 0..100u32 {
            queue.enqueue(vec![7u8; 256]).expect("enqueue"); // 25_600 字节
        }
        queue.flush().expect("flush");

        let bytes = queue.disk_bytes().expect("disk_bytes");
        let rows = queue.pending_disk().expect("disk_rows");
        assert!(bytes <= 4 * 1024, "总字节必须回落到阈值内: {bytes}");
        assert!(rows > 0 && rows < 100, "应淘汰部分旧批次: {rows}");
        assert_eq!(bytes, (rows as u64) * 256);

        let audit = queue.drain_audit();
        assert!(
            audit.iter().any(|event| matches!(
                event,
                QueueAudit::Evicted { reason, .. } if reason.contains("max_db_bytes")
            )),
            "必须产生环形覆盖审计: {audit:?}"
        );

        let survivors = queue.take_batch(1000).expect("take_batch");
        assert_eq!(survivors.len(), rows);
        assert_eq!(
            survivors.last().map(|b| b.seq),
            Some(100),
            "必须保留最新批次"
        );
        assert_eq!(survivors.first().map(|b| b.seq), Some(101 - rows as u64));
        queue.close().expect("close");
    }

    /// 在线模式下少量数据不落盘（留在内存待异步上传）。
    #[test]
    fn online_mode_keeps_rows_in_memory_only() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let queue = open(dir.path(), "gw-online", &clock);
        queue.set_online(true);
        for i in 0..5u32 {
            queue
                .enqueue(format!("mem-{i}").into_bytes())
                .expect("enqueue");
        }
        assert_eq!(queue.pending_mem(), 5, "在线时数据留在内存");
        assert_eq!(queue.pending_disk().expect("disk"), 0, "在线时不落盘");
        assert_eq!(queue.pending().expect("pending"), 5);

        // 显式 flush 后才落到磁盘。
        assert_eq!(queue.flush().expect("flush"), 5);
        assert_eq!(queue.pending_mem(), 0);
        assert_eq!(queue.pending_disk().expect("disk"), 5);
        queue.close().expect("close");
    }

    /// 并发入队：seq 全局唯一、总数正确、顺序可重建。
    #[test]
    fn concurrent_enqueue_assigns_unique_monotonic_seqs() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let queue = Arc::new(open(dir.path(), "gw-concurrent", &clock));

        let mut handles = Vec::new();
        for t in 0..4u32 {
            let queue = Arc::clone(&queue);
            handles.push(thread::spawn(move || {
                let mut seqs = Vec::new();
                for i in 0..50u32 {
                    seqs.push(
                        queue
                            .enqueue(format!("t{t}-{i:02}").into_bytes())
                            .expect("enqueue"),
                    );
                }
                seqs
            }));
        }
        let mut all: Vec<u64> = Vec::new();
        for handle in handles {
            all.extend(handle.join().expect("join"));
        }

        assert_eq!(all.len(), 200);
        let mut sorted = all.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 200, "seq 不得重复");
        assert_eq!(
            sorted,
            (1..=200u64).collect::<Vec<u64>>(),
            "seq 必须全局单调连续"
        );
        assert_eq!(queue.pending().expect("pending"), 200);

        let batch = queue.take_batch(200).expect("take_batch");
        assert_eq!(batch.len(), 200);
        for pair in batch.windows(2) {
            assert!(pair[0].seq < pair[1].seq, "顺序必须可重建");
        }
        queue.close().expect("close");
    }

    /// 库文件隔离：`queue.db` 独立存在，目录中不得出现 `telemetry.db`，且 WAL 侧车已生成。
    #[test]
    fn queue_db_file_is_isolated_from_telemetry_db() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let queue = open(dir.path(), "gw-isolation", &clock);
        queue.set_online(false);
        queue.enqueue(b"payload".to_vec()).expect("enqueue");

        assert_eq!(
            queue.queue_db_path().file_name().and_then(|n| n.to_str()),
            Some("queue.db"),
            "库文件名必须是 queue.db"
        );
        assert!(
            dir.path().join(QUEUE_DB_FILE_NAME).exists(),
            "queue.db 必须存在"
        );
        assert!(
            !dir.path().join(TELEMETRY_DB_FILE_NAME).exists(),
            "不得与 task 18 的 telemetry.db 共用库文件"
        );
        assert!(
            dir.path().join("queue.db-wal").exists(),
            "WAL 模式必须生成 -wal 侧车文件"
        );
        queue.close().expect("close");
    }

    /// 错误路径：不可写目录 → StorageError(4000)；非法配置 → ConfigError(2000)。
    #[test]
    fn open_rejects_unwritable_path_and_invalid_config() {
        let dir = tempdir();

        // 1) 用一个「文件」冒充目录 → 创建目录失败 → StorageError(4000)。
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, b"not a directory").expect("write blocker");
        let config = QueueConfig::new(blocker.join(QUEUE_DB_FILE_NAME), "gw-err").expect("config");
        let err = OfflineQueue::open(config, Arc::new(SystemClock)).expect_err("must fail");
        assert_eq!(err.error_code(), ERR_STORAGE);
        assert!(matches!(err, DaemonError::StorageError(_)), "err: {err}");

        // 2) max_mem_rows = 0 → ConfigError(2000)。
        let mut config = cfg(dir.path(), "gw-err");
        config.max_mem_rows = 0;
        let err = OfflineQueue::open(config, Arc::new(SystemClock)).expect_err("must fail");
        assert_eq!(err.error_code(), ERR_CONFIG);
        assert!(matches!(err, DaemonError::ConfigError(_)), "err: {err}");
        assert!(err.to_string().contains("max_mem_rows"));

        // 3) 空网关标识 → ConfigError(2000)。
        let err =
            QueueConfig::new(dir.path().join(QUEUE_DB_FILE_NAME), "  ").expect_err("must fail");
        assert_eq!(err.error_code(), ERR_CONFIG);

        // 4) 库文件名写成 telemetry.db → ConfigError(2000)（隔离红线）。
        let err = QueueConfig::new(dir.path().join(TELEMETRY_DB_FILE_NAME), "gw-err")
            .expect_err("must fail");
        assert_eq!(err.error_code(), ERR_CONFIG);
        assert!(err.to_string().contains("queue.db"), "err: {err}");
    }

    /// 批量事务：一次 flush 后磁盘行数 == 内存原行数。
    #[test]
    fn flush_writes_all_memory_rows_in_one_transaction() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let queue = open(dir.path(), "gw-batch", &clock);
        queue.set_online(true);
        for i in 0..7u32 {
            queue
                .enqueue(format!("b-{i}").into_bytes())
                .expect("enqueue");
        }
        assert_eq!(queue.pending_mem(), 7);
        assert_eq!(queue.flush().expect("flush"), 7, "一次事务写入 7 行");
        assert_eq!(
            queue.pending_disk().expect("disk"),
            7,
            "磁盘行数 == 内存原行数"
        );
        assert_eq!(queue.pending_mem(), 0);
        assert_eq!(
            queue.flush().expect("flush empty"),
            0,
            "空内存 flush 返回 0"
        );

        let audit = queue.drain_audit();
        assert!(
            audit
                .iter()
                .any(|event| matches!(event, QueueAudit::Flushed { rows: 7 })),
            "必须记录落盘审计: {audit:?}"
        );
        assert_eq!(queue.take_batch(100).expect("take").len(), 7);
        queue.close().expect("close");
    }

    /// WAL 模式生效 + ack 删除已确认行（位点只增不减）。
    #[test]
    fn wal_mode_enabled_and_ack_deletes_confirmed_rows() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let queue = open(dir.path(), "gw-wal", &clock);
        queue.set_online(false);
        for i in 0..10u32 {
            queue
                .enqueue(format!("w-{i}").into_bytes())
                .expect("enqueue");
        }
        assert_eq!(queue.pending_disk().expect("disk"), 10);

        // 只读连接校验 journal_mode 为 wal（不参与写，不违反单写者）。
        let ro =
            Connection::open_with_flags(queue.queue_db_path(), OpenFlags::SQLITE_OPEN_READ_ONLY)
                .expect("open read-only");
        let mode: String = ro
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("pragma");
        assert_eq!(mode.to_lowercase(), "wal", "必须启用 WAL");
        drop(ro);

        queue.ack_up_to(5).expect("ack");
        assert_eq!(queue.pending_disk().expect("disk"), 5, "已确认行必须删除");
        assert_eq!(queue.ack_seq(), 5);
        // 位点只增不减：更小的 seq 不得回退位点。
        queue.ack_up_to(3).expect("ack idempotent");
        assert_eq!(queue.ack_seq(), 5);

        let batch = queue.take_batch(10).expect("take");
        assert_eq!(batch.len(), 5);
        assert_eq!(batch.first().map(|b| b.seq), Some(6));
        assert_eq!(batch.last().map(|b| b.seq), Some(10));
        queue.close().expect("close");
    }

    /// 时钟契约：SystemClock 单调可读；ManualClock 可 set / advance。
    #[test]
    fn clocks_report_injected_and_system_time() {
        let system = SystemClock::new();
        let first = system.now_ns();
        assert!(first > 0, "系统时钟必须为正: {first}");

        let manual = ManualClock::new(T0_NS);
        assert_eq!(manual.now(), T0_NS);
        assert_eq!(manual.now_ns(), T0_NS);
        manual.advance(DAY_NS);
        assert_eq!(manual.now_ns(), T0_NS + DAY_NS);
        manual.set(T0_NS + 10 * DAY_NS);
        assert_eq!(manual.now_ns(), T0_NS + 10 * DAY_NS);
        // 克隆共享同一时间源。
        let cloned = manual.clone();
        cloned.advance(-DAY_NS);
        assert_eq!(manual.now_ns(), T0_NS + 9 * DAY_NS);

        let as_clock: Arc<dyn Clock> = Arc::new(manual.clone());
        assert_eq!(as_clock.now_ns(), T0_NS + 9 * DAY_NS);
    }

    /// 队列关闭后入队被拒（StorageError），且 Drop 不 panic。
    #[test]
    fn closed_queue_rejects_enqueue_without_panic() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let queue = open(dir.path(), "gw-closed", &clock);
        queue.set_online(false);
        queue.enqueue(b"x".to_vec()).expect("enqueue");
        queue.close().expect("close");
        // 幂等关闭。
        queue.close().expect("close idempotent");

        let err = queue.enqueue(b"y".to_vec()).expect_err("must fail");
        assert_eq!(err.error_code(), ERR_STORAGE);
        drop(queue);
    }

    // ---- task 54：幂等去重 + 背压水位 ----

    /// 模拟接收端：按幂等键去重（服务端幂等校验的测试替身）。
    struct MockServer {
        /// 收到的投递总次数（含重放）。
        delivered: usize,
        /// 命中已处理键的重复投递次数。
        duplicates: usize,
        /// 已处理的幂等键集合（净效果）。
        processed: HashSet<String>,
    }

    impl MockServer {
        fn new() -> Self {
            Self {
                delivered: 0,
                duplicates: 0,
                processed: HashSet::new(),
            }
        }

        fn deliver(&mut self, key: &str) {
            self.delivered += 1;
            if !self.processed.insert(key.to_string()) {
                self.duplicates += 1;
            }
        }
    }

    /// 记录型 AckSink：成功持久化并记录调用顺序。
    struct RecordingAckSink {
        acks: Mutex<Vec<u64>>,
    }

    impl AckSink for RecordingAckSink {
        fn persist_ack(&self, seq: u64) -> DaemonResult<()> {
            self.acks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(seq);
            Ok(())
        }
    }

    /// 失败型 AckSink：模拟「落 Ack 时崩溃」（持久化失败 → 位点不得推进）。
    struct FailingAckSink;

    impl AckSink for FailingAckSink {
        fn persist_ack(&self, _seq: u64) -> DaemonResult<()> {
            Err(storage_err("ack persist failed (fault injection)"))
        }
    }

    /// 默认水位参数：硬上限 65536、高水位 8192（task 54 决议值）。
    #[test]
    fn default_water_marks_are_8192_high_and_65536_hard() {
        let dir = tempdir();
        let config = cfg(dir.path(), "gw-default");
        assert_eq!(config.max_mem_rows, 65_536, "硬上限默认 65536");
        assert_eq!(config.mem_high_water_rows, 8_192, "高水位默认 8192");
        assert_eq!(config.high_water_rows(), 8_192);

        // 高水位可配置（且必须 > 0）。
        let dir2 = tempdir();
        let mut config2 = cfg(dir2.path(), "gw-override");
        config2.mem_high_water_rows = 4_096;
        config2.validate().expect("valid");
        assert_eq!(config2.high_water_rows(), 4_096);
        config2.mem_high_water_rows = 0;
        assert_eq!(
            config2
                .validate()
                .expect_err("zero high water")
                .error_code(),
            ERR_CONFIG
        );
    }

    /// 高水位必须被夹紧到硬上限的 4/5 以内（保证降级路径先于拒绝路径生效）。
    #[test]
    fn high_water_rows_is_clamped_to_four_fifths_of_hard_cap() {
        let dir = tempdir();
        let mut config = cfg(dir.path(), "gw-clamp");
        config.max_mem_rows = 10;
        config.mem_high_water_rows = 100;
        assert_eq!(config.high_water_rows(), 8, "夹紧到 4/5");
        config.max_mem_rows = 1;
        assert_eq!(config.high_water_rows(), 1, "至少为 1");
    }

    /// 幂等键 = gateway_id + batch_seq；u64 全域不丢精度。
    #[test]
    fn idempotency_key_is_gateway_plus_seq() {
        assert_eq!(idempotency_key("gw-1", 42), "gw-1:42");
        let batch = QueuedBatch {
            seq: 7,
            gateway_id: "gw-2".to_string(),
            payload: Vec::new(),
            enqueued_ns: 0,
        };
        assert_eq!(batch.idempotency_key(), "gw-2:7");
        assert_eq!(idempotency_key("gw", u64::MAX), format!("gw:{}", u64::MAX));
    }

    /// seq 来源可注入（测试可控）；补发全程幂等键稳定。
    #[test]
    fn seq_source_is_injectable_and_idempotency_keys_stay_stable() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let hooks = QueueHooks {
            seq_source: Some(Arc::new(AtomicSeqSource::new(1_000_000))),
            ack_sink: None,
        };
        let queue = OfflineQueue::open_with_hooks(
            cfg(dir.path(), "gw-seq"),
            Arc::new(clock.clone()),
            hooks,
        )
        .expect("open with injected seq source");
        queue.set_online(false);
        let s1 = queue.enqueue(b"a".to_vec()).expect("enqueue 1");
        let s2 = queue.enqueue(b"b".to_vec()).expect("enqueue 2");
        assert_eq!((s1, s2), (1_000_000, 1_000_001), "seq 必须来自注入来源");

        // 两次补发（重放）得到完全相同的幂等键。
        let first: Vec<String> = queue
            .replay_batch(10)
            .expect("replay 1")
            .iter()
            .map(|b| b.idempotency_key())
            .collect();
        let second: Vec<String> = queue
            .replay_batch(10)
            .expect("replay 2")
            .iter()
            .map(|b| b.idempotency_key())
            .collect();
        assert_eq!(first, second, "补发全程幂等键必须稳定");
        assert_eq!(first, vec!["gw-seq:1000000", "gw-seq:1000001"]);

        queue.ack_up_to(s2).expect("ack");
        queue.close().expect("close");
    }

    /// 重放无重复键：崩溃（未 ack）后重启重放同一批，接收端按幂等键去重后净效果
    /// = 每键只处理一次（mock 发送端见到的幂等键集合无重复）。
    #[test]
    fn replay_after_crash_without_ack_dedups_by_idempotency_key() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let mut server = MockServer::new();

        // 第一次补发（不确认任何批次）。
        let queue = open(dir.path(), "gw-replay", &clock);
        queue.set_online(false);
        for i in 0..10u32 {
            queue
                .enqueue(format!("r-{i}").into_bytes())
                .expect("enqueue offline");
        }
        for batch in queue.replay_batch(100).expect("replay 1") {
            server.deliver(&batch.idempotency_key());
        }
        assert_eq!(server.processed.len(), 10);
        // 模拟进程崩溃：不调用 close，直接 drop（位点停在 0）。
        drop(queue);

        // 重启后重放：HWM 仍为 0 → 同一批条目再次出现，幂等键逐条相同。
        let reopened = open(dir.path(), "gw-replay", &clock);
        assert_eq!(reopened.ack_seq(), 0, "未落 Ack，重启后位点必须保持 0");
        for batch in reopened.replay_batch(100).expect("replay 2") {
            server.deliver(&batch.idempotency_key());
        }
        assert_eq!(server.delivered, 20, "重放确实发生了第二次投递");
        assert_eq!(server.duplicates, 10, "重复投递由接收端按键吸收");
        assert_eq!(
            server.processed.len(),
            10,
            "mock 接收端见到的幂等键集合无重复（净效果 = 只处理一次）"
        );
        reopened.close().expect("close");
    }

    /// HWM 跳过：已确认位点之前的条目不再出现在补发结果里。
    #[test]
    fn replay_batch_skips_entries_before_high_water_mark() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let queue = open(dir.path(), "gw-hwm", &clock);
        queue.set_online(false);
        for i in 0..8u32 {
            queue.enqueue(format!("h-{i}").into_bytes()).expect("e");
        }
        queue.ack_up_to(5).expect("ack");
        let batch = queue.replay_batch(100).expect("replay");
        let seqs: Vec<u64> = batch.iter().map(|b| b.seq).collect();
        assert_eq!(seqs, vec![6, 7, 8], "必须跳过 seq <= HWM 的条目");
        assert!(
            batch.iter().all(|b| b.seq > queue.ack_seq()),
            "补发条目必须全部在位点之后"
        );
        queue.close().expect("close");
    }

    /// Ack 语义（注入 AckSink）：持久化失败 → 内存位点不推进、数据不删。
    #[test]
    fn failing_ack_sink_blocks_memory_pointer_advance() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let hooks = QueueHooks {
            seq_source: None,
            ack_sink: Some(Arc::new(FailingAckSink)),
        };
        let queue = OfflineQueue::open_with_hooks(
            cfg(dir.path(), "gw-ack-fail"),
            Arc::new(clock.clone()),
            hooks,
        )
        .expect("open with failing ack sink");
        queue.set_online(false);
        for i in 0..3u32 {
            queue.enqueue(format!("f-{i}").into_bytes()).expect("e");
        }

        let err = queue.ack_up_to(2).expect_err("落 Ack 失败必须报错");
        assert_eq!(err.error_code(), ERR_STORAGE);
        assert_eq!(queue.ack_seq(), 0, "持久化失败 → 内存位点不得推进");
        assert_eq!(queue.pending().expect("pending"), 3, "数据不得删除");
        assert_eq!(
            queue.replay_batch(10).expect("replay").len(),
            3,
            "失败后数据仍完整可补发（重放由幂等键兜底）"
        );
        queue.close().expect("close");
    }

    /// Ack 语义（注入 AckSink）：确认记录先落（sink 收到持久化调用），
    /// 持久化成功后内存位点才推进、HWM 之前的条目被跳过。
    #[test]
    fn ack_sink_persists_before_memory_pointer_advances() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let sink = Arc::new(RecordingAckSink {
            acks: Mutex::new(Vec::new()),
        });
        let hooks = QueueHooks {
            seq_source: None,
            ack_sink: Some(Arc::clone(&sink) as Arc<dyn AckSink>),
        };
        let queue = OfflineQueue::open_with_hooks(
            cfg(dir.path(), "gw-ack-ok"),
            Arc::new(clock.clone()),
            hooks,
        )
        .expect("open with recording ack sink");
        queue.set_online(false);
        for i in 0..3u32 {
            queue.enqueue(format!("a-{i}").into_bytes()).expect("e");
        }

        queue.ack_up_to(2).expect("ack");
        assert_eq!(
            sink.acks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_slice(),
            &[2],
            "确认记录必须先持久化（sink 收到调用且仅一次）"
        );
        assert_eq!(queue.ack_seq(), 2, "持久化成功后才推进内存位点");
        let seqs: Vec<u64> = queue
            .replay_batch(10)
            .expect("replay")
            .iter()
            .map(|b| b.seq)
            .collect();
        assert_eq!(seqs, vec![3], "已确认位点之前的条目必须被跳过");
        queue.close().expect("close");
    }

    /// 高水位降级路径：内存超水位后数据流向磁盘（降级），生产者不阻塞、零丢失，
    /// 内存永不突破硬上限。
    #[test]
    fn high_water_degradation_keeps_producer_unblocked_and_no_data_loss() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let mut config = cfg(dir.path(), "gw-degrade");
        config.max_mem_rows = 4; // 高水位 = 3
        let queue = OfflineQueue::open(config, Arc::new(clock.clone())).expect("open");
        queue.set_online(true);

        for i in 0..20u32 {
            queue
                .enqueue(format!("d-{i:02}").into_bytes())
                .expect("enqueue 绝不阻塞、绝不失败");
        }
        assert_eq!(queue.pending().expect("pending"), 20, "零丢失");
        assert!(
            queue.pending_mem() <= 4,
            "内存不得超过硬上限: {}",
            queue.pending_mem()
        );
        assert!(
            queue.pending_disk().expect("disk") > 0,
            "超水位数据必须已降级落盘"
        );

        let batch = queue.replay_batch(100).expect("replay");
        assert_eq!(batch.len(), 20);
        for (i, b) in batch.iter().enumerate() {
            assert_eq!(b.seq, i as u64 + 1, "按序、无重复、无空洞");
        }
        queue.close().expect("close");
    }

    /// 大数红线：JSON 元信息里 `batch_seq` / `payload_bytes` 一律字符串编码。
    #[test]
    fn batch_meta_json_encodes_large_batch_seq_as_string() {
        // 2^53 + 1：超出 JavaScript Number 安全整数域的边界。
        let big = 9_007_199_254_740_993u64;
        let batch = QueuedBatch {
            seq: big,
            gateway_id: "gw\"big\\".to_string(),
            payload: vec![0u8; 512],
            enqueued_ns: 0,
        };
        let json = batch_meta_json(&batch);
        assert!(
            json.contains("\"batch_seq\":\"9007199254740993\""),
            "batch_seq 必须字符串编码: {json}"
        );
        assert!(
            json.contains("\"payload_bytes\":\"512\""),
            "count 类字段一律字符串: {json}"
        );
        assert!(
            json.contains("\"gateway_id\":\"gw\\\"big\\\\\""),
            "gateway_id 必须 JSON 转义: {json}"
        );
        assert!(
            !json.contains("\"batch_seq\":9"),
            "禁止把 batch_seq 编码为数值: {json}"
        );
    }
}
