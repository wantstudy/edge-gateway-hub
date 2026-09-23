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
//! 3. **水位三级**：高水位（默认硬上限的 4/5）触发强制落盘；硬上限（行数 / 字节）在**落盘失败
//!    或落盘后仍超限**时丢弃最旧批次，并记 [`QueueAudit::DroppedOldest`]——**绝不静默丢弃**。
//! 4. **环形覆盖**：每个落盘事务末尾做一次维护——先按 `retention_ns`（注入时钟，测试零 sleep）
//!    淘汰过期批次，再按 `max_db_bytes` 从最旧开始逐条淘汰，均记 [`QueueAudit::Evicted`]。
//! 5. **幂等键 = (gateway_id, batch_seq)**。`batch_seq` 由原子计数器单调递增分配，并在
//!    `queue_meta.last_seq` 持久化，保证进程重启后不回退、不复用。
//! 6. **续传**：`ack_up_to(seq)` 推进 high-water mark 并落盘（`queue_meta.ack_seq`）＋删除
//!    `seq <= ack` 的行；`open` 时读回两者，重启后从 `ack_seq + 1` 继续补发——无重复、无空洞。
//! 7. **顺序**：`take_batch` 合并磁盘与内存结果后按 `seq` 升序排序去重，同设备内严格有序。
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

/// 默认内存队列行数硬上限（10 万条）。
pub const DEFAULT_MAX_MEM_ROWS: usize = 100_000;

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

// ---- 配置 ----

/// 队列配置（非法值在 `open` 时转 `ConfigError`，错误码 2000）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueConfig {
    /// 队列库路径（文件名**必须**是 `queue.db`，禁止写成 `telemetry.db`）。
    pub db_path: PathBuf,
    /// 网关标识（幂等键组成之一，非空）。
    pub gateway_id: String,
    /// 内存队列行数硬上限（> 0）。
    pub max_mem_rows: usize,
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

    /// 高水位行数（硬上限的 4/5，至少 1）。
    #[must_use]
    pub fn high_water_rows(&self) -> usize {
        (self.max_mem_rows.saturating_mul(HIGH_WATER_NUM) / HIGH_WATER_DEN).max(1)
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
}

/// 审计事件：丢弃 / 淘汰 / 落盘**必须留痕**，绝不静默。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueAudit {
    /// 触达硬上限且落盘无法缓解 → 丢弃最旧的队首批次。
    DroppedOldest {
        /// 被丢弃的批次序号。
        seq: u64,
        /// 丢弃原因（可诊断）。
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
    next_seq: AtomicU64,
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
            .field("next_seq", &self.next_seq.load(Ordering::SeqCst))
            .field("online", &self.online.load(Ordering::SeqCst))
            .field("pending_mem", &self.mem().rows.len())
            .field("closed", &self.closed.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl OfflineQueue {
    /// 打开（或创建）队列库并启动单写线程。
    ///
    /// # Errors
    /// - 配置非法 → `ConfigError`（2000）；
    /// - 目录创建 / SQLite 打开 / WAL 启用 / 建表失败 → `StorageError`（4000）。
    pub fn open(cfg: QueueConfig, clock: Arc<dyn Clock>) -> DaemonResult<Self> {
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

        Ok(Self {
            cfg,
            clock,
            mem: Mutex::new(MemState::default()),
            next_seq: AtomicU64::new(init.last_seq.saturating_add(1)),
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
    /// - 在线：先进内存队列；达高水位强制落盘；落盘失败且触达硬上限 → 丢弃最旧 + 审计。
    /// - 离线：**直接落盘**（保证断电也不丢），落盘失败即返回 `StorageError`。
    ///
    /// # Errors
    /// 队列已关闭、或离线且落盘失败 → `StorageError`（4000）。
    pub fn enqueue(&self, payload: Vec<u8>) -> DaemonResult<u64> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(storage_err("enqueue rejected: queue is closed"));
        }
        // `next_seq` 初值 = 持久化 last_seq + 1，故直接取当前值即为本批 seq，
        // 再自增给下一批（fetch_add 返回旧值）。首次入队 seq == last_seq + 1。
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let batch = QueuedBatch {
            seq,
            gateway_id: self.cfg.gateway_id.clone(),
            payload,
            enqueued_ns: self.clock.now_ns(),
        };

        if self.online.load(Ordering::SeqCst) {
            let high_rows = self.cfg.high_water_rows();
            let high_bytes = self.cfg.high_water_bytes();
            let over_high_water = {
                let mut mem = self.mem();
                mem.rows.push_back(batch);
                mem.bytes = mem
                    .bytes
                    .saturating_add(mem.rows.back().map_or(0, |b| b.payload_len()));
                mem.rows.len() >= high_rows || mem.bytes >= high_bytes
            };
            // 高水位：先尝试落盘（能落盘就绝不丢数据）。
            if over_high_water {
                let _ = self.flush();
            }
            // 落盘失败 / 落盘后仍超限 → 丢弃最旧并留审计（绝不静默）。
            self.enforce_mem_cap("memory hard limit reached");
        } else {
            self.send_persist(vec![batch])?;
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

    /// 推进 high-water mark：持久化位点并删除 `seq <= seq` 的批次。
    ///
    /// 位点只增不减；重启后从 `ack_seq + 1` 续传 —— 无重复、无空洞。
    ///
    /// # Errors
    /// 位点持久化失败 → `StorageError`（4000）；失败时内存位点**不推进**，避免丢数据。
    pub fn ack_up_to(&self, seq: u64) -> DaemonResult<()> {
        let current = self.ack_seq.load(Ordering::SeqCst);
        if seq <= current {
            return Ok(());
        }
        self.send_cmd(|resp| Cmd::Ack { seq, resp })?;
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
    /// 用于验证「落盘不可写 → 硬上限丢弃最旧并留审计」的降级路径（计划 QA Error 场景）。
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

    /// 硬上限守卫：超出即丢弃最旧批次并留审计（至少保留最新一条）。
    fn enforce_mem_cap(&self, reason: &str) {
        let mut dropped = Vec::new();
        {
            let mut mem = self.mem();
            while mem.rows.len() > self.cfg.max_mem_rows || mem.bytes > self.cfg.max_mem_bytes {
                if mem.rows.len() <= 1 {
                    // 单条负载即超过上限：保留最新一条，避免清空后无数据可传。
                    break;
                }
                match mem.rows.pop_front() {
                    Some(oldest) => {
                        mem.bytes = mem.bytes.saturating_sub(oldest.payload_len());
                        dropped.push(oldest.seq);
                    }
                    None => break,
                }
            }
        }
        for seq in dropped {
            push_audit(
                &self.audit,
                QueueAudit::DroppedOldest {
                    seq,
                    reason: reason.to_string(),
                },
            );
        }
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

    /// 断言审计中存在被丢弃的 seq 列表（按发生顺序）。
    fn dropped_seqs(audit: &[QueueAudit]) -> Vec<u64> {
        audit
            .iter()
            .filter_map(|event| match event {
                QueueAudit::DroppedOldest { seq, .. } => Some(*seq),
                _ => None,
            })
            .collect()
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

    /// QA Error：硬上限 → 丢弃最旧 + 审计留痕，且保留最新数据。
    #[test]
    fn hard_limit_drops_oldest_with_audit_and_keeps_newest() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let mut config = cfg(dir.path(), "gw-hard-limit");
        config.max_mem_rows = 5;
        let queue = OfflineQueue::open(config, Arc::new(clock.clone())).expect("open");
        // 模拟「落盘不可写」：高水位落盘必定失败，只能按策略丢弃最旧。
        queue.set_persist_failure(true);

        for i in 0..20u32 {
            let seq = queue
                .enqueue(format!("p-{i:02}").into_bytes())
                .expect("enqueue");
            assert_eq!(seq, u64::from(i) + 1);
        }
        assert_eq!(queue.pending_mem(), 5, "硬上限后只保留 5 条最新数据");
        assert_eq!(queue.pending().expect("pending"), 5);

        let audit = queue.drain_audit();
        let dropped = dropped_seqs(&audit);
        assert_eq!(dropped.len(), 15, "丢弃 15 条最旧数据且全部留痕");
        assert_eq!(
            dropped,
            (1..=15u64).collect::<Vec<u64>>(),
            "丢弃顺序应为从最旧开始"
        );
        assert!(
            audit.iter().any(|event| matches!(
                event,
                QueueAudit::DroppedOldest { reason, .. } if reason.contains("hard limit")
            )),
            "审计 reason 需可诊断: {audit:?}"
        );

        let survivors = queue.take_batch(10).expect("take_batch");
        assert_eq!(
            survivors.iter().map(|b| b.seq).collect::<Vec<_>>(),
            vec![16, 17, 18, 19, 20],
            "保留的必须是最新数据"
        );
        assert_eq!(
            survivors.last().map(|b| b.payload.clone()),
            Some(b"p-19".to_vec())
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
}
