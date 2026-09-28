//! B 档审计回执**批次账本**（task 48 服务端二次校验：幂等 + 序号连续性仲裁）。
//!
//! # 背景（B 档契约）
//!
//! 默认拓扑下客户业务数据直连客户自己的 Broker、不经厂商云，因此消息级云端校验
//! 没有落点。补救 = 网关侧签名 + 单调序号 + 控制通道上报回执。服务端接收回执后：
//!
//! 1. **验签**（[`crate::receipt::verify_receipt_signature`]，在 service 层、本模块**之前**）
//! 2. **幂等仲裁**（本模块）：批次头表主键 + `INSERT OR IGNORE` + 检查 `rows_affected`
//! 3. **序号区间检测**（本模块）：与该 `device_mid` 最近一次 accepted 回执比较
//! 4. 告警记录（本模块 `audit_receipt_warning` 表）→ 返回体 `warnings` 字段
//!
//! # 红线对应
//!
//! - **幂等红线**：`idempotency_key` 是**批次级**标识（一批多码共用）——**不能**建
//!   行级唯一索引；仲裁用**批次头表主键**（单事务内 `INSERT OR IGNORE` 后读
//!   `rows_affected`），**禁止**「分页查询 + 内存过滤」式判定（存在竞态窗口）。
//! - **安全红线**：本模块**不做验签**——调用方必须已验签；伪造回执绝不能借
//!   「与上一条同区间」的幂等路径拿到 `accepted=true`。
//! - **大数红线**：序号以 `u64` 承载，落库前经 `i64::try_from` 边界检查（越界 →
//!   `Storage` 错误，**绝不**静默截断）。
//!
//! # 自建连接与迁移（不碰既有表结构）
//!
//! 按任务约束，本模块**自建** SQLite 连接并自建迁移，只创建 / 读写**自己的三张表**，
//! 不触碰 `store.rs` 的既有表结构：
//!
//! - `audit_receipt_batch`：批次头表（主键 = 批次键，幂等仲裁点）
//! - `audit_receipt_device_cursor`：设备级序号 cursor（每 `device_mid` 一行）
//! - `audit_receipt_warning`：告警记录（跳空 / 回退 / 重叠）
//!
//! 连接创建或迁移失败时**降级为内存库**（幂等记忆退化为进程生命周期；绝不 panic），
//! 连内存库都失败（理论上仅 OOM）则所有操作返回 `Storage` 错误。
//!
//! # 比较语义（红线 8 的字面实现）
//!
//! 跳空 / 回退 / 重叠的比较基准是**该 `device_mid` 最近一次 accepted 回执**
//! （设备级 cursor，跨租约延续；`last_seq_to` 单调取 max 作为窗口前沿）：
//!
//! - **跳空**：`seq_from > last_seq_to + 1`
//! - **回退**：`seq_to < last_seq_to`（窗口整体后退）
//! - **重叠**：`seq_from <= last_seq_to` 且非回退（可接受，记录）
//! - 无历史且 `seq_from > 1` → 前缀缺失，按**跳空**告警
//!
//! 三种异常都写 `audit_receipt_warning` 表，并通过返回值进入响应体 `warnings` 字段。
//! 告警是**人工核实**信号（写入 `audit_log` 由 service 层完成），不自动封禁。

use std::fmt;

use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::error::{LicenseError, LicenseResult};
use crate::proto::GapKind;

/// 批次键的域分隔前缀（服务端内部约定；改动会使重启后的幂等记忆失效，仅此影响）。
pub const RECEIPT_BATCH_KEY_DOMAIN: &str = "iotdaq.audit.batch.v1";

// ============================================================================
// 批次键
// ============================================================================

/// 追加一个长度前缀字段：`|name=<字节长度>:<value>`（与 [`crate::receipt`] 同风格，
/// 保证含 `|` / 非 ASCII 的 `mid` 也不会产生键歧义）。
fn push_len_field(out: &mut String, name: &str, value: &str) {
    out.push('|');
    out.push_str(name);
    out.push_str(&value.len().to_string());
    out.push(':');
    out.push_str(value);
}

/// 计算批次头主键：`SHA-256(domain ++ 长度前缀字段)` 的十六进制。
///
/// 批次身份 = `(device_mid, lease_id, seq_from, seq_to)`——同一批（同区间）重放
/// （无论 `sig` / `payload_digest` / `ts` 是否相同）命中同一主键 → 幂等重放。
/// 用哈希而非拼接原文做主键：`mid` 是客户端可控字符串，直接拼接会让主键长度失控。
#[must_use]
pub fn batch_key(device_mid: &str, lease_id: &str, seq_from: u64, seq_to: u64) -> String {
    let mut out = String::with_capacity(128);
    out.push_str(RECEIPT_BATCH_KEY_DOMAIN);
    push_len_field(&mut out, "mid", device_mid);
    push_len_field(&mut out, "lease_id", lease_id);
    push_len_field(&mut out, "seq_from", &seq_from.to_string());
    push_len_field(&mut out, "seq_to", &seq_to.to_string());

    let mut hasher = Sha256::new();
    hasher.update(out.as_bytes());
    let digest = hasher.finalize();

    let mut hex = String::with_capacity(digest.len() * 2);
    for b in digest {
        hex.push_str(&format!("{b:02x}"));
    }
    hex
}

// ============================================================================
// 数据结构
// ============================================================================

/// 一次已验签回执的落账请求（由 service 层在**验签通过之后**构造）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchRecord<'a> {
    /// 设备机位码（比较基准的归属者）。
    pub device_mid: &'a str,
    /// 租约 ID（批次身份的一部分；同一设备换租约后序号历史延续）。
    pub lease_id: &'a str,
    /// 序号区间起点（含）。
    pub seq_from: u64,
    /// 序号区间终点（含）。
    pub seq_to: u64,
    /// 区间内消息条数。
    pub count: u64,
    /// 业务负载摘要（不透明字符串，仅落账）。
    pub payload_digest: &'a str,
    /// 客户端声明时间（unix 秒，已过时序窗校验）。
    pub ts: i64,
    /// 服务端受理时间（unix 秒）。
    pub accepted_at: i64,
}

/// 批次落账结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchOutcome {
    /// 幂等重放：批次头已存在（`rows_affected == 0`）。**不**改 cursor、**不**告警。
    Replay,
    /// 新批次入库。
    Recorded {
        /// 连续性判定（跳空 / 回退 / 重叠 / 连续）。
        gap: GapKind,
        /// 告警明细（与 `audit_receipt_warning` 表逐条对应；连续时为空）。
        warnings: Vec<String>,
        /// 判定时的 cursor 前沿（无历史为 0；供 service 层写 `audit_log` 细节）。
        last_seq_to: i64,
    },
}

/// 一条告警记录（测试与运营查询用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarningRow {
    /// 异常类别（`gap` / `overlap`，与 [`GapKind::as_str`] 同域）。
    pub kind: String,
    /// 触发批次的区间起点。
    pub seq_from: i64,
    /// 触发批次的区间终点。
    pub seq_to: i64,
    /// 判定时的 cursor 前沿。
    pub last_seq_to: i64,
    /// 告警明细（与响应体 `warnings` 字段同文）。
    pub detail: String,
    /// 记录时间（unix 秒）。
    pub created_at: i64,
}

impl fmt::Display for WarningRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.detail)
    }
}

/// 一条告警记录的**列表形态**（管理端异常页；在 [`WarningRow`] 之上补充
/// `rowid` / `device_mid` / `lease_id`，供跨设备总览展示与处置定位）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarningListRow {
    /// 告警行 ID（`audit_receipt_warning.rowid`）。
    pub id: i64,
    /// 设备机位码。
    pub device_mid: String,
    /// 租约 ID。
    pub lease_id: String,
    /// 异常类别（`gap` / `overlap`）。
    pub kind: String,
    /// 触发批次的区间起点。
    pub seq_from: i64,
    /// 触发批次的区间终点。
    pub seq_to: i64,
    /// 判定时的 cursor 前沿。
    pub last_seq_to: i64,
    /// 告警明细。
    pub detail: String,
    /// 记录时间（unix 秒）。
    pub created_at: i64,
}

// ============================================================================
// 连续性判定（纯函数，单测全覆盖）
// ============================================================================

/// 依据 cursor 前沿判定新批次的连续性（红线 8 的字面语义）。
///
/// `last` 为 `(last_seq_from, last_seq_to)`；`None` 表示该设备尚无 accepted 回执。
#[must_use]
pub fn decide_gap(last: Option<(i64, i64)>, seq_from: i64, seq_to: i64) -> (GapKind, Vec<String>) {
    match last {
        None => {
            if seq_from > 1 {
                (
                    GapKind::Gap,
                    vec![format!(
                        "missing-prefix: no prior accepted receipt; seq_from {seq_from} > 1, window [1, {}] unreported",
                        seq_from - 1
                    )],
                )
            } else {
                (GapKind::None, Vec::new())
            }
        }
        Some((_, last_to)) => {
            if seq_from > last_to.saturating_add(1) {
                (
                    GapKind::Gap,
                    vec![format!(
                        "gap: expected seq_from {} (last_seq_to {last_to} + 1), got {seq_from}",
                        last_to.saturating_add(1)
                    )],
                )
            } else if seq_to < last_to {
                (
                    GapKind::Overlap,
                    vec![format!(
                        "regress: seq_to {seq_to} < last_seq_to {last_to}; window moved backwards"
                    )],
                )
            } else if seq_from <= last_to {
                (
                    GapKind::Overlap,
                    vec![format!(
                        "overlap: seq_from {seq_from} <= last_seq_to {last_to}; ranges intersect (accepted, recorded)"
                    )],
                )
            } else {
                (GapKind::None, Vec::new())
            }
        }
    }
}

// ============================================================================
// 账本
// ============================================================================

/// 回执批次账本：批次头幂等 + 设备 cursor + 告警记录。
///
/// 线程安全：内部单连接 + `parking_lot::Mutex`，`record_batch` 在单事务内完成
/// 「批次头仲裁 → cursor 读取 → 判定 → 告警落库 → cursor 更新」，并发重放批次
/// 中**恰好一个**返回 [`BatchOutcome::Recorded`]，其余返回 [`BatchOutcome::Replay`]。
pub struct ReceiptLedger {
    /// 单连接。`None` = 初始化彻底失败（磁盘 + 内存均不可用），所有操作报 `Storage`。
    conn: Mutex<Option<Connection>>,
}

impl ReceiptLedger {
    /// 打开账本：`Some(path)` → 文件库（重启后幂等记忆保留）；`None` → 内存库。
    ///
    /// **绝不 panic**：文件库打开 / 迁移失败 → 自动降级内存库；内存库也失败
    /// （仅 OOM 级故障）→ 连接置空，后续操作返回 `Storage` 错误。
    #[must_use]
    pub fn open(path: Option<&str>) -> Self {
        let primary = match path {
            Some(p) => Connection::open(p).ok().filter(|c| migrate(c).is_ok()),
            None => Connection::open_in_memory()
                .ok()
                .filter(|c| migrate(c).is_ok()),
        };
        // 主库不可用 → 内存库兜底（幂等记忆退化为进程生命周期）。
        let conn = primary
            .or_else(|| Connection::open_in_memory().ok())
            .filter(|c| migrate(c).is_ok());
        ReceiptLedger {
            conn: Mutex::new(conn),
        }
    }

    /// 内存账本（测试专用便捷入口）。
    #[must_use]
    pub fn open_in_memory() -> Self {
        Self::open(None)
    }

    /// 幂等落账：批次头仲裁 + 连续性判定 + 告警落库 + cursor 更新（单事务）。
    ///
    /// # Errors
    /// - 初始化失败的账本 → [`LicenseError::Storage`]
    /// - 序号超出 `i64` 范围（理论上已被上游验签前的解析拦截）→ [`LicenseError::Storage`]
    /// - SQLite 错误 → [`LicenseError::Storage`]
    pub fn record_batch(&self, rec: &BatchRecord<'_>) -> LicenseResult<BatchOutcome> {
        let seq_from = i64::try_from(rec.seq_from)
            .map_err(|e| LicenseError::Storage(format!("receipt seq_from out of range: {e}")))?;
        let seq_to = i64::try_from(rec.seq_to)
            .map_err(|e| LicenseError::Storage(format!("receipt seq_to out of range: {e}")))?;
        let count = i64::try_from(rec.count)
            .map_err(|e| LicenseError::Storage(format!("receipt count out of range: {e}")))?;
        let key = batch_key(rec.device_mid, rec.lease_id, rec.seq_from, rec.seq_to);

        let mut guard = self.conn.lock();
        let conn = guard
            .as_mut()
            .ok_or_else(|| LicenseError::Storage("receipt ledger is unavailable".into()))?;
        let tx = conn
            .transaction()
            .map_err(|e| LicenseError::Storage(format!("ledger begin tx failed: {e}")))?;

        // ---- 幂等仲裁（红线 5）：批次头表主键 + INSERT OR IGNORE + rows_affected ----
        let inserted = tx
            .execute(
                "INSERT OR IGNORE INTO audit_receipt_batch
                 (batch_key, device_mid, lease_id, seq_from, seq_to, count, payload_digest, ts, accepted_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    key,
                    rec.device_mid,
                    rec.lease_id,
                    seq_from,
                    seq_to,
                    count,
                    rec.payload_digest,
                    rec.ts,
                    rec.accepted_at
                ],
            )
            .map_err(|e| LicenseError::Storage(format!("ledger batch insert failed: {e}")))?;
        if inserted == 0 {
            // 幂等重放：不改 cursor、不告警（告警只对**新批次**产生）。
            tx.commit()
                .map_err(|e| LicenseError::Storage(format!("ledger commit failed: {e}")))?;
            return Ok(BatchOutcome::Replay);
        }

        // ---- 连续性判定（红线 8）：与该 device_mid 最近一次 accepted 回执比较 ----
        let last: Option<(i64, i64)> = tx
            .query_row(
                "SELECT last_seq_from, last_seq_to FROM audit_receipt_device_cursor
                 WHERE device_mid = ?1",
                params![rec.device_mid],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(|e| LicenseError::Storage(format!("ledger cursor read failed: {e}")))?;
        let (gap, warnings) = decide_gap(last, seq_from, seq_to);
        let last_seq_to = last.map(|(_, to)| to).unwrap_or(0);

        // ---- 告警落库（跳空 / 回退 / 重叠；人工核实信号，不自动封禁）----
        for detail in &warnings {
            tx.execute(
                "INSERT INTO audit_receipt_warning
                 (device_mid, lease_id, kind, seq_from, seq_to, last_seq_to, detail, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    rec.device_mid,
                    rec.lease_id,
                    gap.as_str(),
                    seq_from,
                    seq_to,
                    last_seq_to,
                    detail,
                    rec.accepted_at
                ],
            )
            .map_err(|e| LicenseError::Storage(format!("ledger warning insert failed: {e}")))?;
        }

        // ---- cursor 更新：last_seq_to 单调取 max（窗口前沿不因回退 / 重叠后退）----
        let new_to = last.map(|(_, to)| to.max(seq_to)).unwrap_or(seq_to);
        tx.execute(
            "INSERT INTO audit_receipt_device_cursor (device_mid, last_seq_from, last_seq_to, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(device_mid) DO UPDATE SET
               last_seq_from = ?2, last_seq_to = ?3, updated_at = ?4",
            params![rec.device_mid, seq_from, new_to, rec.accepted_at],
        )
        .map_err(|e| LicenseError::Storage(format!("ledger cursor update failed: {e}")))?;

        tx.commit()
            .map_err(|e| LicenseError::Storage(format!("ledger commit failed: {e}")))?;
        Ok(BatchOutcome::Recorded {
            gap,
            warnings,
            last_seq_to,
        })
    }

    // ------------------------------------------------------------------
    // 只读查询（测试与运营核查）
    // ------------------------------------------------------------------

    /// 批次头总数（幂等仲裁点上的记录数）。
    ///
    /// # Errors
    /// SQLite 错误 / 账本不可用 → [`LicenseError::Storage`]。
    pub fn batch_count(&self) -> LicenseResult<i64> {
        let guard = self.conn.lock();
        let conn = guard
            .as_ref()
            .ok_or_else(|| LicenseError::Storage("receipt ledger is unavailable".into()))?;
        conn.query_row("SELECT COUNT(*) FROM audit_receipt_batch", [], |row| {
            row.get(0)
        })
        .map_err(|e| LicenseError::Storage(format!("ledger batch count failed: {e}")))
    }

    /// 指定设备是否存在某批次键（幂等命中核查）。
    ///
    /// # Errors
    /// SQLite 错误 / 账本不可用 → [`LicenseError::Storage`]。
    pub fn has_batch(
        &self,
        device_mid: &str,
        lease_id: &str,
        seq_from: u64,
        seq_to: u64,
    ) -> LicenseResult<bool> {
        let key = batch_key(device_mid, lease_id, seq_from, seq_to);
        let guard = self.conn.lock();
        let conn = guard
            .as_ref()
            .ok_or_else(|| LicenseError::Storage("receipt ledger is unavailable".into()))?;
        conn.query_row(
            "SELECT 1 FROM audit_receipt_batch WHERE batch_key = ?1",
            params![key],
            |_| Ok(()),
        )
        .optional()
        .map(|opt| opt.is_some())
        .map_err(|e| LicenseError::Storage(format!("ledger batch lookup failed: {e}")))
    }

    /// 指定设备的告警记录（按时间升序）。
    ///
    /// # Errors
    /// SQLite 错误 / 账本不可用 → [`LicenseError::Storage`]。
    pub fn warnings_for(&self, device_mid: &str) -> LicenseResult<Vec<WarningRow>> {
        let guard = self.conn.lock();
        let conn = guard
            .as_ref()
            .ok_or_else(|| LicenseError::Storage("receipt ledger is unavailable".into()))?;
        let mut stmt = conn
            .prepare(
                "SELECT kind, seq_from, seq_to, last_seq_to, detail, created_at
                 FROM audit_receipt_warning WHERE device_mid = ?1 ORDER BY id ASC",
            )
            .map_err(|e| LicenseError::Storage(format!("ledger warning query failed: {e}")))?;
        let rows = stmt
            .query_map(params![device_mid], |row| {
                Ok(WarningRow {
                    kind: row.get(0)?,
                    seq_from: row.get(1)?,
                    seq_to: row.get(2)?,
                    last_seq_to: row.get(3)?,
                    detail: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })
            .map_err(|e| LicenseError::Storage(format!("ledger warning query failed: {e}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| LicenseError::Storage(format!("ledger warning read failed: {e}")))
    }

    /// 指定设备的 cursor（最近一次 accepted 回执的区间前沿）。
    ///
    /// # Errors
    /// SQLite 错误 / 账本不可用 → [`LicenseError::Storage`]。
    pub fn cursor_of(&self, device_mid: &str) -> LicenseResult<Option<(i64, i64)>> {
        let guard = self.conn.lock();
        let conn = guard
            .as_ref()
            .ok_or_else(|| LicenseError::Storage("receipt ledger is unavailable".into()))?;
        conn.query_row(
            "SELECT last_seq_from, last_seq_to FROM audit_receipt_device_cursor
             WHERE device_mid = ?1",
            params![device_mid],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|e| LicenseError::Storage(format!("ledger cursor read failed: {e}")))
    }

    /// 分页列出**跨设备全部**告警记录（管理端 `GET /admin/receipts/anomalies`）。
    ///
    /// 与 [`Self::warnings_for`] 的差异：不限定设备（管理后台总览页），按 `id` 降序
    /// （最新异常在前），并携带 `rowid` / `device_mid` / `lease_id` 供列表展示。
    ///
    /// # Errors
    /// SQLite 错误 / 账本不可用 → [`LicenseError::Storage`]。
    pub fn list_warnings(&self, page: u32, page_size: u32) -> LicenseResult<Vec<WarningListRow>> {
        let page = page.max(1);
        let page_size = page_size.max(1);
        let offset = i64::from(page.saturating_sub(1).saturating_mul(page_size));
        let guard = self.conn.lock();
        let conn = guard
            .as_ref()
            .ok_or_else(|| LicenseError::Storage("receipt ledger is unavailable".into()))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, device_mid, lease_id, kind, seq_from, seq_to, last_seq_to,
                        detail, created_at
                 FROM audit_receipt_warning
                 ORDER BY id DESC
                 LIMIT ?1 OFFSET ?2",
            )
            .map_err(|e| LicenseError::Storage(format!("ledger warning query failed: {e}")))?;
        let rows = stmt
            .query_map(params![i64::from(page_size), offset], |row| {
                Ok(WarningListRow {
                    id: row.get(0)?,
                    device_mid: row.get(1)?,
                    lease_id: row.get(2)?,
                    kind: row.get(3)?,
                    seq_from: row.get(4)?,
                    seq_to: row.get(5)?,
                    last_seq_to: row.get(6)?,
                    detail: row.get(7)?,
                    created_at: row.get(8)?,
                })
            })
            .map_err(|e| LicenseError::Storage(format!("ledger warning query failed: {e}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| LicenseError::Storage(format!("ledger warning read failed: {e}")))
    }

    /// 统计告警记录总数（跨设备；管理端总览聚合）。
    ///
    /// # Errors
    /// SQLite 错误 / 账本不可用 → [`LicenseError::Storage`]。
    pub fn count_warnings(&self) -> LicenseResult<u64> {
        let guard = self.conn.lock();
        let conn = guard
            .as_ref()
            .ok_or_else(|| LicenseError::Storage("receipt ledger is unavailable".into()))?;
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM audit_receipt_warning", [], |row| {
                row.get(0)
            })
            .map_err(|e| LicenseError::Storage(format!("ledger warning count failed: {e}")))?;
        u64::try_from(count)
            .map_err(|_| LicenseError::Storage(format!("warning count is negative: {count}")))
    }
}

impl fmt::Debug for ReceiptLedger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 绝不打印连接与库内容（账本含回执业务摘要）。
        f.debug_struct("ReceiptLedger").finish_non_exhaustive()
    }
}

// ============================================================================
// 迁移（本模块自建表；不触碰 store.rs 既有表结构）
// ============================================================================

/// 创建本模块的三张表（幂等：`IF NOT EXISTS`）。
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        r#"
        -- 批次头表：主键即幂等仲裁点（批次键 = SHA-256(mid|lease|seq_from|seq_to)）。
        -- 一批多码共用一个批次头：无行级唯一索引，重放命中主键即判定。
        CREATE TABLE IF NOT EXISTS audit_receipt_batch (
            batch_key      TEXT PRIMARY KEY,
            device_mid     TEXT NOT NULL,
            lease_id       TEXT NOT NULL,
            seq_from       INTEGER NOT NULL,
            seq_to         INTEGER NOT NULL,
            count          INTEGER NOT NULL,
            payload_digest TEXT NOT NULL,
            ts             INTEGER NOT NULL,
            accepted_at    INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_audit_batch_device
            ON audit_receipt_batch(device_mid, accepted_at);

        -- 设备级序号 cursor：比较基准 = 该 device_mid 最近一次 accepted 回执
        -- （last_seq_to 单调取 max，跨租约延续）。
        CREATE TABLE IF NOT EXISTS audit_receipt_device_cursor (
            device_mid    TEXT PRIMARY KEY,
            last_seq_from INTEGER NOT NULL,
            last_seq_to   INTEGER NOT NULL,
            updated_at    INTEGER NOT NULL
        );

        -- 告警记录：跳空 / 回退 / 重叠（人工核实信号）。
        CREATE TABLE IF NOT EXISTS audit_receipt_warning (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            device_mid  TEXT NOT NULL,
            lease_id    TEXT NOT NULL,
            kind        TEXT NOT NULL,
            seq_from    INTEGER NOT NULL,
            seq_to      INTEGER NOT NULL,
            last_seq_to INTEGER NOT NULL,
            detail      TEXT NOT NULL,
            created_at  INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_audit_warning_device
            ON audit_receipt_warning(device_mid, created_at);
        "#,
    )?;
    Ok(())
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    /// 构造落账请求的便捷入口。
    fn rec<'a>(
        mid: &'a str,
        lease: &'a str,
        from: u64,
        to: u64,
        digest: &'a str,
    ) -> BatchRecord<'a> {
        BatchRecord {
            device_mid: mid,
            lease_id: lease,
            seq_from: from,
            seq_to: to,
            count: to - from + 1,
            payload_digest: digest,
            ts: 1_700_000_000,
            accepted_at: 1_700_000_100,
        }
    }

    // ---------------- 批次键 ----------------

    /// 批次键：同批次恒等；任一身份字段不同则不同；与 ts / digest / sig 无关。
    #[test]
    fn batch_key_is_stable_per_batch_identity() {
        let a = batch_key("mid-1", "lease-1", 1, 100);
        let b = batch_key("mid-1", "lease-1", 1, 100);
        assert_eq!(a, b, "同批次键必须恒等（幂等重放的命中条件）");
        assert_ne!(batch_key("mid-2", "lease-1", 1, 100), a, "mid 不同");
        assert_ne!(batch_key("mid-1", "lease-2", 1, 100), a, "lease 不同");
        assert_ne!(batch_key("mid-1", "lease-1", 1, 101), a, "seq_to 不同");
        assert_ne!(batch_key("mid-1", "lease-1", 2, 100), a, "seq_from 不同");
        // 键是 64 位十六进制（SHA-256）。
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// 批次键含长度前缀：`|` 出现在字段值里不产生键歧义。
    #[test]
    fn batch_key_length_prefix_is_unambiguous() {
        let a = batch_key("ab", "c", 1, 2);
        let b = batch_key("a", "bc", 1, 2);
        assert_ne!(a, b, "不同字段切分必须产生不同键");
        let c = batch_key("x|mid=1:y", "c", 1, 2);
        let d = batch_key("x", "c", 1, 2);
        assert_ne!(c, d, "值内含分隔符样式不得歧义");
    }

    // ---------------- 连续性判定（纯函数） ----------------

    /// 连续 / 跳空 / 回退 / 重叠 / 前缀缺失五条判定路径。
    #[test]
    fn decide_gap_covers_all_five_paths() {
        // 无历史且 from==1 → None。
        let (gap, w) = decide_gap(None, 1, 10);
        assert_eq!(gap, GapKind::None);
        assert!(w.is_empty());
        // 无历史且 from>1 → 前缀缺失 = Gap。
        let (gap, w) = decide_gap(None, 5, 9);
        assert_eq!(gap, GapKind::Gap);
        assert!(w[0].starts_with("missing-prefix:"), "{}", w[0]);
        // 连续：11..20（前沿 10）→ None。
        let (gap, w) = decide_gap(Some((1, 10)), 11, 20);
        assert_eq!(gap, GapKind::None);
        assert!(w.is_empty());
        // 跳空：25..30（前沿 20）→ Gap。
        let (gap, w) = decide_gap(Some((11, 20)), 25, 30);
        assert_eq!(gap, GapKind::Gap);
        assert!(w[0].starts_with("gap:"), "{}", w[0]);
        assert!(w[0].contains("expected seq_from 21"), "{}", w[0]);
        // 回退：15..18（前沿 30）→ Overlap（regress 明细）。
        let (gap, w) = decide_gap(Some((21, 30)), 15, 18);
        assert_eq!(gap, GapKind::Overlap);
        assert!(w[0].starts_with("regress:"), "{}", w[0]);
        // 重叠（非回退）：25..40（前沿 30）→ Overlap（overlap 明细）。
        let (gap, w) = decide_gap(Some((21, 30)), 25, 40);
        assert_eq!(gap, GapKind::Overlap);
        assert!(w[0].starts_with("overlap:"), "{}", w[0]);
    }

    // ---------------- 落账：幂等 / 判定 / 告警 ----------------

    /// 首批落账：Recorded + cursor 建立；同批次重放：Replay，不二次告警、不二次落账。
    #[test]
    fn record_batch_replay_is_exactly_once() {
        let ledger = ReceiptLedger::open_in_memory();
        let r = rec("mid-a", "lease-a", 1, 10, "d1");

        let out1 = ledger.record_batch(&r).expect("first record");
        match &out1 {
            BatchOutcome::Recorded { gap, warnings, .. } => {
                assert_eq!(*gap, GapKind::None);
                assert!(warnings.is_empty(), "连续批次不得产生告警");
            }
            other => panic!("首批必须 Recorded，实际: {other:?}"),
        }
        assert_eq!(ledger.batch_count().expect("count"), 1);
        assert_eq!(
            ledger.cursor_of("mid-a").expect("cursor"),
            Some((1, 10)),
            "cursor 必须建立为首批区间"
        );

        // 完全相同批次重放（同 mid/lease/from/to，digest 可不同）→ Replay。
        let out2 = ledger.record_batch(&r).expect("replay");
        assert_eq!(out2, BatchOutcome::Replay);
        // 不同 digest 的同区间也是同一批次（批次身份不含 digest）。
        let out3 = ledger
            .record_batch(&rec("mid-a", "lease-a", 1, 10, "d2"))
            .expect("replay with other digest");
        assert_eq!(out3, BatchOutcome::Replay);

        assert_eq!(
            ledger.batch_count().expect("count"),
            1,
            "重放不得新增批次头"
        );
        assert_eq!(
            ledger.cursor_of("mid-a").expect("cursor"),
            Some((1, 10)),
            "重放不得移动 cursor"
        );
        assert!(
            ledger.warnings_for("mid-a").expect("warnings").is_empty(),
            "重放不得产生告警"
        );
    }

    /// 跳空 / 回退 / 重叠三种异常均落告警表，且明细与返回的 warnings 一致。
    #[test]
    fn record_batch_persists_warnings_for_all_anomalies() {
        let ledger = ReceiptLedger::open_in_memory();
        // 首批 1..10。
        ledger
            .record_batch(&rec("mid-w", "lease-w", 1, 10, "d"))
            .expect("first");

        // 跳空 25..30。
        let out = ledger
            .record_batch(&rec("mid-w", "lease-w", 25, 30, "d"))
            .expect("gap");
        let BatchOutcome::Recorded {
            gap,
            warnings,
            last_seq_to,
        } = out
        else {
            panic!("必须 Recorded");
        };
        assert_eq!(gap, GapKind::Gap);
        assert_eq!(last_seq_to, 10);
        assert_eq!(warnings.len(), 1);

        // 回退 12..18（前沿 30）。
        let out = ledger
            .record_batch(&rec("mid-w", "lease-w", 12, 18, "d"))
            .expect("regress");
        let BatchOutcome::Recorded { gap, warnings, .. } = out else {
            panic!("必须 Recorded");
        };
        assert_eq!(gap, GapKind::Overlap);
        assert!(warnings[0].starts_with("regress:"), "{}", warnings[0]);

        // 重叠（非回退）31..40。
        let out = ledger
            .record_batch(&rec("mid-w", "lease-w", 31, 40, "d"))
            .expect("overlap");
        let BatchOutcome::Recorded { gap, warnings, .. } = out else {
            panic!("必须 Recorded");
        };
        assert_eq!(gap, GapKind::None, "31..40 紧跟前沿 30，连续");
        let _ = warnings;

        // 重叠：20..45 与前沿 40 相交。
        let out = ledger
            .record_batch(&rec("mid-w", "lease-w", 20, 45, "d"))
            .expect("overlap2");
        let BatchOutcome::Recorded { gap, warnings, .. } = out else {
            panic!("必须 Recorded");
        };
        assert_eq!(gap, GapKind::Overlap);
        assert!(warnings[0].starts_with("overlap:"), "{}", warnings[0]);

        // 告警表逐条对应：gap + regress + overlap = 3 条。
        let rows = ledger.warnings_for("mid-w").expect("rows");
        assert_eq!(rows.len(), 3, "三种异常各 1 条，实际: {rows:?}");
        assert_eq!(rows[0].kind, "gap");
        assert_eq!(rows[1].kind, "overlap"); // regress 归 overlap 族
        assert_eq!(rows[2].kind, "overlap");
        // 明细与响应 warnings 字段同文（detail 即告警原文）。
        // 跳空批次 25..30 的前沿是首批的 10 → 期望起点为 11。
        assert!(
            rows[0].detail.contains("expected seq_from 11"),
            "{}",
            rows[0].detail
        );
        assert_eq!(rows[0].seq_from, 25);
        assert_eq!(rows[0].last_seq_to, 10);
        assert!(rows[1].detail.starts_with("regress:"), "{}", rows[1].detail);
        assert!(rows[2].detail.starts_with("overlap:"), "{}", rows[2].detail);
    }

    /// cursor 是**设备级**：同设备跨租约延续序号历史；不同设备互不影响。
    #[test]
    fn cursor_is_per_device_and_survives_lease_change() {
        let ledger = ReceiptLedger::open_in_memory();
        // 设备 A：lease-1 上 1..10。
        ledger
            .record_batch(&rec("mid-a", "lease-1", 1, 10, "d"))
            .expect("a1");
        // 设备 A 换租约 lease-2：11..20 仍应判定为连续（跨租约延续）。
        let out = ledger
            .record_batch(&rec("mid-a", "lease-2", 11, 20, "d"))
            .expect("a2");
        let BatchOutcome::Recorded { gap, .. } = out else {
            panic!("必须 Recorded")
        };
        assert_eq!(gap, GapKind::None, "同设备跨租约必须延续序号历史");

        // 设备 B 从 1..5 起步：不得受设备 A 的 cursor 影响。
        let out = ledger
            .record_batch(&rec("mid-b", "lease-3", 1, 5, "d"))
            .expect("b1");
        let BatchOutcome::Recorded { gap, warnings, .. } = out else {
            panic!("必须 Recorded")
        };
        assert_eq!(gap, GapKind::None);
        assert!(warnings.is_empty());
        assert_eq!(ledger.cursor_of("mid-a").expect("cursor-a"), Some((11, 20)));
        assert_eq!(ledger.cursor_of("mid-b").expect("cursor-b"), Some((1, 5)));
    }

    /// cursor 前沿单调取 max：回退批次（新批次 Recorded）不得把窗口前沿拉回去。
    #[test]
    fn cursor_frontier_is_monotonic_against_regress() {
        let ledger = ReceiptLedger::open_in_memory();
        ledger
            .record_batch(&rec("mid-m", "lease-m", 1, 100, "d"))
            .expect("first");
        // 回退批次 40..50：Recorded（新批次键）但前沿保持 100。
        let out = ledger
            .record_batch(&rec("mid-m", "lease-m", 40, 50, "d"))
            .expect("regress");
        let BatchOutcome::Recorded { gap, .. } = out else {
            panic!("必须 Recorded")
        };
        assert_eq!(gap, GapKind::Overlap);
        assert_eq!(ledger.cursor_of("mid-m").expect("cursor"), Some((40, 100)));
        // 后续 101..110 仍以 100 为前沿 → 连续。
        let out = ledger
            .record_batch(&rec("mid-m", "lease-m", 101, 110, "d"))
            .expect("next");
        let BatchOutcome::Recorded { gap, warnings, .. } = out else {
            panic!("必须 Recorded")
        };
        assert_eq!(gap, GapKind::None);
        assert!(warnings.is_empty());
    }

    /// 大数红线：2^53 以上的序号无损落账与判定（JSON number 早已丢精度的区间）。
    #[test]
    fn big_seq_numbers_round_trip_losslessly() {
        let ledger = ReceiptLedger::open_in_memory();
        let base: u64 = 9_007_199_254_740_993; // 2^53 + 1
                                               // 无历史的大数起步：按「前缀缺失」判 Gap（先验证大数比较不溢出）。
        let out = ledger
            .record_batch(&rec("mid-big", "lease-big", base, base + 9, "d"))
            .expect("big batch");
        let BatchOutcome::Recorded { gap, .. } = out else {
            panic!("必须 Recorded")
        };
        assert_eq!(gap, GapKind::Gap, "无历史且 seq_from > 1 必须判 Gap");
        assert_eq!(
            ledger.cursor_of("mid-big").expect("cursor"),
            Some((base as i64, (base + 9) as i64)),
            "大数必须无损落库"
        );
        // 续窗 base+10..base+19 → 连续（大数 +1 比较无溢出）。
        let out = ledger
            .record_batch(&rec("mid-big", "lease-big", base + 10, base + 19, "d"))
            .expect("big next");
        let BatchOutcome::Recorded { gap, warnings, .. } = out else {
            panic!("必须 Recorded")
        };
        assert_eq!(gap, GapKind::None);
        assert!(warnings.is_empty());
    }

    /// 前缀缺失：无历史且 seq_from > 1 → Gap 告警。
    #[test]
    fn first_batch_with_missing_prefix_is_gap_with_warning() {
        let ledger = ReceiptLedger::open_in_memory();
        let out = ledger
            .record_batch(&rec("mid-p", "lease-p", 5, 9, "d"))
            .expect("first");
        let BatchOutcome::Recorded { gap, warnings, .. } = out else {
            panic!("必须 Recorded")
        };
        assert_eq!(gap, GapKind::Gap);
        assert_eq!(warnings.len(), 1);
        assert_eq!(ledger.warnings_for("mid-p").expect("rows").len(), 1);
    }

    /// **并发安全（红线 5 的核心证据）**：N 个线程并发提交同一批次，
    /// 恰好 1 个 `Recorded`、其余全部 `Replay`，批次头恰好 1 行。
    #[test]
    fn concurrent_duplicate_batches_arbitrate_to_exactly_one_record() {
        let ledger = std::sync::Arc::new(ReceiptLedger::open_in_memory());
        const THREADS: usize = 8;

        let handles: Vec<_> = (0..THREADS)
            .map(|i| {
                let ledger = std::sync::Arc::clone(&ledger);
                thread::spawn(move || {
                    // 每线程稍作错峰，放大竞态窗口（若存在）。
                    thread::sleep(Duration::from_millis(i as u64 % 3));
                    ledger.record_batch(&rec("mid-c", "lease-c", 1, 100, "d"))
                })
            })
            .collect();

        let mut recorded = 0usize;
        let mut replayed = 0usize;
        for h in handles {
            match h.join().expect("thread must not panic") {
                Ok(BatchOutcome::Recorded { .. }) => recorded += 1,
                Ok(BatchOutcome::Replay) => replayed += 1,
                Err(e) => panic!("并发落账不得失败: {e}"),
            }
        }
        assert_eq!(recorded, 1, "恰好一个批次被记录");
        assert_eq!(replayed, THREADS - 1, "其余全部幂等重放");
        assert_eq!(ledger.batch_count().expect("count"), 1, "批次头恰好 1 行");
        assert_eq!(ledger.cursor_of("mid-c").expect("cursor"), Some((1, 100)));
    }

    /// 并发**不同**批次（同设备）：全部 Recorded，cursor 前沿为最大 seq_to。
    #[test]
    fn concurrent_distinct_batches_all_record_and_advance_frontier() {
        let ledger = std::sync::Arc::new(ReceiptLedger::open_in_memory());
        let handles: Vec<_> = (0..4)
            .map(|i| {
                let ledger = std::sync::Arc::clone(&ledger);
                let from = 1 + i * 10;
                thread::spawn(move || {
                    ledger.record_batch(&rec("mid-d", "lease-d", from, from + 9, "d"))
                })
            })
            .collect();
        for h in handles {
            let out = h
                .join()
                .expect("thread must not panic")
                .expect("distinct batch");
            let BatchOutcome::Recorded { gap, .. } = out else {
                panic!("不同批次必须全部 Recorded: {out:?}")
            };
            let _ = gap;
        }
        assert_eq!(ledger.batch_count().expect("count"), 4);
        assert_eq!(
            ledger.cursor_of("mid-d").expect("cursor").map(|(_, to)| to),
            Some(40)
        );
    }

    /// 迁移幂等：同一文件库重复 open 不报错、数据保留。
    #[test]
    fn reopen_file_ledger_keeps_batches_and_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ledger.db");
        let path_str = path.to_str().expect("utf-8 path");

        {
            let ledger = ReceiptLedger::open(Some(path_str));
            ledger
                .record_batch(&rec("mid-f", "lease-f", 1, 10, "d"))
                .expect("first");
        }
        // 重新打开：迁移幂等 + 批次头仍在（重启后幂等记忆保留）。
        let ledger = ReceiptLedger::open(Some(path_str));
        assert_eq!(
            ledger.batch_count().expect("count"),
            1,
            "重启后批次头必须保留"
        );
        let out = ledger
            .record_batch(&rec("mid-f", "lease-f", 1, 10, "d"))
            .expect("replay");
        assert_eq!(out, BatchOutcome::Replay, "重启后同批次仍须判重放");
        assert_eq!(ledger.batch_count().expect("count"), 1);
    }

    /// 越界防护：超出 `i64` 的序号 → `Storage` 错误，**绝不**静默截断为负数。
    #[test]
    fn out_of_range_seq_returns_storage_error_not_truncation() {
        let ledger = ReceiptLedger::open_in_memory();
        let huge = u64::MAX; // > i64::MAX
        let mut r = rec("mid-o", "lease-o", 1, 10, "d");
        r.seq_to = huge;
        let err = ledger.record_batch(&r).expect_err("必须拒绝");
        assert!(err.to_string().contains("seq_to"), "{err}");
        assert_eq!(
            ledger.batch_count().expect("count"),
            0,
            "失败批次不得留下半途状态"
        );
    }
}
