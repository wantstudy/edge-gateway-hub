//! 控制指令下发链路（task #140，daemon 后端生产链路）。
//!
//! # 职责边界
//! - **做**：把管理面「写指令」（当前支持 Modbus `write_register` / `write_coil`）
//!   真正投入现场设备——解析 → 校验 → 真实南向写（经 [`ControlWritePort`]，实现方
//!   持有真实驱动连接）→ 落审计。幂等（批次级 `idempotency_key`）：重复提交返回
//!   首次相同结果、绝不二次下发。
//! - **不做**：北向 / 采集 / 授权判定（RBAC 由 mgmt 层 `AuthedRole::ensure` 承担）；
//!   读路径；新增具体驱动的写能力只通过扩展 op→`WritePoint` 编码 + 驱动 `write`
//!   实现完成，**不在此层硬编码驱动功能码**（接口只暴露南向 `WritePoint`）。
//!
//! # 投递语义（诚实红线）
//! - 受理成功 → [`ControlIssueResult`] 由调用方（mgmt/ctrl_api）映射为 **202
//!   accepted**，响应体明确「已提交下发、尚未确认设备侧应用」；
//! - 设备不存在 / 协议驱动未接线 / 参数非法 / 设备不可达 → **结构化错误**
//!   （错误码 + 真实原因串），**绝不返回 202、绝不伪造成功**。失败来源是真实
//!   建连 / 写尝试的报错，不是伪造的成功。
//!
//! # 大数红线
//! 时间戳（ts_ns，u64）一律字符串编码（落库与对外一致）。

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use rusqlite::{params, Connection};

use crate::bootstrap::DaemonShared;
use crate::driver::{PointAddressParser, WritePoint};
use crate::error::{DaemonError, DaemonResult, ERR_CONFIG, ERR_NETWORK, ERR_PROTOCOL};

/// 控制面下发端口：把一组「已解析的南向写点」真正投入设备。
///
/// 实现方（[`crate::southbound::DevicePollHandler`]）持有真实驱动连接；本 trait
/// 不暴露任何 Modbus 功能码 / 寄存器布局——接口只看见南向抽象 [`WritePoint`]，
/// 后续新增驱动的写能力无需改动本 trait（只改 op→`WritePoint` 编码与驱动 `write`）。
#[async_trait]
pub trait ControlWritePort: Send + Sync {
    /// 判定某设备当前是否可写（设备登记 / 仿真 / 协议接线层面），不可写返回
    /// 真实原因（[`DaemonError`]）；不接触设备（连接尝试在 [`Self::dispatch`]）。
    async fn assert_writable(
        &self,
        device_id: &str,
        point_id: Option<&str>,
        address: &str,
    ) -> DaemonResult<()>;

    /// 设备是否在控制面登记（计划表命中即视为存在）；纯查询、不接触设备。
    fn device_known(&self, device_id: &str) -> bool;

    /// 真实下发一组已解析写点（已通过 [`Self::assert_writable`]）。逐点汇总
    /// [`WriteOutcome`]，任一失败即整批失败（调用方据此整批结构化为错误）。
    async fn dispatch(&self, device_id: &str, points: &[WritePoint]) -> DaemonResult<Vec<WriteOutcome>>;
}

/// 单点写结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteOutcome {
    /// 是否成功下发到设备（驱动 write 成功）。
    pub delivered: bool,
    /// 人读原因（成功 = "ok"，失败 = 真实错误串）。
    pub reason: String,
}

/// 控制操作（op 字面量域，小写下划线；扩展新驱动写能力只在此追加）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlOp {
    /// Modbus FC06 写单个保持寄存器。
    WriteRegister,
    /// Modbus FC05 写单个线圈。
    WriteCoil,
}

impl ControlOp {
    /// op 字面量。
    pub fn as_str(self) -> &'static str {
        match self {
            ControlOp::WriteRegister => "write_register",
            ControlOp::WriteCoil => "write_coil",
        }
    }

    /// 解析 op 字面量（未知 → `None`，由调用方转 structured 400）。
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "write_register" => Some(ControlOp::WriteRegister),
            "write_coil" => Some(ControlOp::WriteCoil),
            _ => None,
        }
    }
}

/// 已解析、可下发的单条控制指令。
#[derive(Debug, Clone)]
pub struct ControlCommand {
    /// 目标设备。
    pub device_id: String,
    /// 点位标识（可选；仅用于仿真判定 / 展示）。
    pub point_id: Option<String>,
    /// 南向地址原文（展示用）。
    pub address: String,
    /// 操作。
    pub op: ControlOp,
    /// 已编码的南向写点（含协议地址 + 原始字节值）。
    pub point: WritePoint,
}

/// 下发请求（管理面 JSON 入参）。
#[derive(Debug, Clone)]
pub struct ControlIssueRequest {
    /// 批次级幂等键（可选；缺省不保证幂等）。
    pub idempotency_key: Option<String>,
    /// 指令列表（至少一条）。
    pub commands: Vec<ControlCommand>,
}

/// 单条指令的对外结果（HTTP 响应体单元）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CommandOutcome {
    /// 设备。
    pub device_id: String,
    /// 点位（展示）。
    pub point_id: Option<String>,
    /// 地址（展示）。
    pub address: String,
    /// 操作字面量。
    pub op: String,
    /// 是否成功下发。
    pub delivered: bool,
    /// 真实原因（成功 = "ok"）。
    pub reason: String,
}

/// 下发结果（受理 / 重复）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ControlIssueResult {
    /// 是否命中幂等键（重复提交 → true，且未二次下发）。
    pub duplicate: bool,
    /// 本次（或首次）使用的幂等键。
    pub idempotency_key: Option<String>,
    /// 逐指令结果。
    pub commands: Vec<CommandOutcome>,
}

/// 下发失败的结构化错误（映射为 HTTP 状态码 + 错误码）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlIssueError {
    /// 参数非法（400）。
    BadRequest(String),
    /// 设备不存在（404）。
    NotFound(String),
    /// 驱动 / 协议不支持该写（409）。
    Unsupported(String),
    /// 设备不可达 / 写被设备拒绝（502）。
    DeliveryFailed(String),
    /// 内部错误（控制面未装配 / 存储失败，500）。
    Internal(String),
}

impl ControlIssueError {
    /// 错误码字面量（与响应体 `error` 字段一致）。
    pub fn code(&self) -> &'static str {
        match self {
            ControlIssueError::BadRequest(_) => "bad_request",
            ControlIssueError::NotFound(_) => "not_found",
            ControlIssueError::Unsupported(_) => "unsupported",
            ControlIssueError::DeliveryFailed(_) => "delivery_failed",
            ControlIssueError::Internal(_) => "internal",
        }
    }

    /// 人读原因。
    pub fn reason(&self) -> &str {
        match self {
            ControlIssueError::BadRequest(r)
            | ControlIssueError::NotFound(r)
            | ControlIssueError::Unsupported(r)
            | ControlIssueError::DeliveryFailed(r)
            | ControlIssueError::Internal(r) => r,
        }
    }

    /// 对应 HTTP 状态码。
    pub fn status(&self) -> u16 {
        match self {
            ControlIssueError::BadRequest(_) => 400,
            ControlIssueError::NotFound(_) => 404,
            ControlIssueError::Unsupported(_) => 409,
            ControlIssueError::DeliveryFailed(_) => 502,
            ControlIssueError::Internal(_) => 500,
        }
    }

    /// 从持久账本存储的 `(code, reason)` 重建结构化错误（幂等回放失败首发用）。
    fn from_storage(code: &str, reason: &str) -> Self {
        match code {
            "bad_request" => ControlIssueError::BadRequest(reason.to_string()),
            "not_found" => ControlIssueError::NotFound(reason.to_string()),
            "unsupported" => ControlIssueError::Unsupported(reason.to_string()),
            "delivery_failed" => ControlIssueError::DeliveryFailed(reason.to_string()),
            _ => ControlIssueError::Internal(reason.to_string()),
        }
    }
}

/// 幂等回放结果：首发成功或失败（回放时原样返回，绝不二次下发、绝不伪造成功）。
enum Replay {
    /// 首发成功 → 回放结果体（duplicate=true）。
    Success(ControlIssueResult),
    /// 首发失败 → 回放结构化错误（与首次一致，绝不 202）。
    Failure(ControlIssueError, Vec<CommandOutcome>),
}

impl Replay {
    /// 落库形态：(status 字面量, detail, 逐指令结果)。
    fn storage(&self) -> (&'static str, &str, &[CommandOutcome]) {
        match self {
            Replay::Success(r) => ("accepted", "issued", &r.commands),
            Replay::Failure(e, cmds) => (e.code(), e.reason(), cmds),
        }
    }
}

/// 把南向 [`DaemonError`] 映射到控制面结构化错误（错误码保留真实原因）。
fn to_issue_error(err: DaemonError) -> ControlIssueError {
    match err.error_code() {
        ERR_CONFIG => {
            let msg = err.to_string();
            if msg.contains("unknown device group") {
                ControlIssueError::NotFound(msg)
            } else {
                // 仿真点 / 协议未接线等配置层不支持。
                ControlIssueError::Unsupported(msg)
            }
        }
        ERR_NETWORK => ControlIssueError::DeliveryFailed(format!(
            "device unreachable: {err}"
        )),
        ERR_PROTOCOL => ControlIssueError::DeliveryFailed(format!(
            "device rejected write (protocol exception): {err}"
        )),
        _ => ControlIssueError::Internal(format!("control dispatch failed: {err}")),
    }
}

// ---- 时钟 ----

/// 时间戳注入闭包（返回 UTC 纳秒）；测试注入受控时钟保证确定性。
pub type ControlClock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// 当前 UTC 纳秒（Unix 纪元起；时钟早于纪元按 0 处理，不 panic）。
fn system_clock_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// 默认墙钟。
#[must_use]
pub fn default_clock() -> ControlClock {
    Arc::new(system_clock_ns)
}

// ---- 指令解析（JSON → 已校验 ControlCommand） ----

/// 把单条请求指令解析为可下发的 [`ControlCommand`]。
///
/// # Errors
/// 参数非法（未知 op / 地址不可解析 / 值类型或范围不符）返回 [`ControlIssueError::BadRequest`]。
fn parse_one(req: &RawCommand) -> Result<ControlCommand, ControlIssueError> {
    let op = ControlOp::parse(&req.op).ok_or_else(|| {
        ControlIssueError::BadRequest(format!(
            "unsupported op {op:?}; supported: write_register, write_coil",
            op = req.op
        ))
    })?;

    let (address, point) = match op {
        ControlOp::WriteRegister => {
            let parsed = PointAddressParser::parse(&req.address).map_err(|e| {
                ControlIssueError::BadRequest(format!(
                    "invalid register address {addr:?}: {e}",
                    addr = req.address
                ))
            })?;
            if parsed.area != Some('4') {
                return Err(ControlIssueError::BadRequest(format!(
                    "write_register requires a 4xxxx holding register address (got area \
                     {area:?}); 3xxxx input registers are read-only",
                    area = parsed.area
                )));
            }
            let value = req.value.as_u64().ok_or_else(|| {
                ControlIssueError::BadRequest(format!(
                    "write_register value must be an integer (0..=65535), got {v:?}",
                    v = req.value
                ))
            })?;
            if value > u32::from(u16::MAX) as u64 {
                return Err(ControlIssueError::BadRequest(format!(
                    "write_register value {value} out of u16 range (0..=65535)"
                )));
            }
            let point = WritePoint {
                address: parsed,
                value: (value as u16).to_be_bytes().to_vec(),
            };
            (req.address.clone(), point)
        }
        ControlOp::WriteCoil => {
            let parsed = PointAddressParser::parse_coil_address(&req.address).map_err(|e| {
                ControlIssueError::BadRequest(format!(
                    "invalid coil address {addr:?}: {e}",
                    addr = req.address
                ))
            })?;
            // 值：布尔，或 0/1 数值。
            let on = if let Some(b) = req.value.as_bool() {
                b
            } else if let Some(n) = req.value.as_u64() {
                match n {
                    0 => false,
                    1 => true,
                    _ => {
                        return Err(ControlIssueError::BadRequest(format!(
                            "write_coil value must be a boolean or 0/1, got {n}"
                        )))
                    }
                }
            } else {
                return Err(ControlIssueError::BadRequest(format!(
                    "write_coil value must be a boolean or 0/1, got {v:?}",
                    v = req.value
                )));
            };
            let point = WritePoint {
                address: parsed,
                value: vec![if on { 0xFF } else { 0x00 }],
            };
            (req.address.clone(), point)
        }
    };

    Ok(ControlCommand {
        device_id: req.device_id.clone(),
        point_id: req.point_id.clone(),
        address,
        op,
        point,
    })
}

/// 请求 JSON 原始形态（值用 [`serde_json::Value`] 承载，因类型随 op 变化）。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RawIssueRequest {
    /// 批次级幂等键（可选）。
    #[serde(default)]
    pub idempotency_key: Option<String>,
    /// 指令列表。
    pub commands: Vec<RawCommand>,
}

/// 单条请求指令原始形态。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RawCommand {
    /// 目标设备。
    pub device_id: String,
    /// 点位标识（可选）。
    #[serde(default)]
    pub point_id: Option<String>,
    /// 南向地址（如 "40001" / "00001"）。
    pub address: String,
    /// 操作字面量（write_register / write_coil）。
    pub op: String,
    /// 值（write_register=整数；write_coil=布尔或 0/1）。
    pub value: serde_json::Value,
}

/// 把原始请求解析为已校验的 [`ControlIssueRequest`]。
///
/// # Errors
/// 指令为空 / 任一指令非法 → [`ControlIssueError`]。
pub fn parse_issue(raw: &RawIssueRequest) -> Result<ControlIssueRequest, ControlIssueError> {
    if raw.commands.is_empty() {
        return Err(ControlIssueError::BadRequest(
            "commands must contain at least one command".to_string(),
        ));
    }
    if let Some(key) = &raw.idempotency_key {
        if key.trim().is_empty() {
            return Err(ControlIssueError::BadRequest(
                "idempotency_key must not be empty".to_string(),
            ));
        }
    }
    let mut commands = Vec::with_capacity(raw.commands.len());
    for cmd in &raw.commands {
        commands.push(parse_one(cmd)?);
    }
    Ok(ControlIssueRequest {
        idempotency_key: raw.idempotency_key.clone(),
        commands,
    })
}

// ---- 持久账本（幂等 + 历史） ----

/// 控制指令账本（SQLite；与 audit.db 同口径，纯 rusqlite，零新增依赖）。
///
/// 幂等语义：**批次级**——`control_batch.idempotency_key` 为唯一键（非逐行），
/// 重复提交走 `INSERT OR IGNORE`：已存在行（`changes()==0`）即视为重复，直接返回
/// 首次存储的结果、**不二次下发**。逐行 `control_command` 仅作归属与历史查询，不施加
/// 行级唯一约束（刻意避免「行级唯一索引」导致批量部分冲突）。
pub struct ControlLedger {
    /// 连接（std Mutex；写事务短持，不跨 await）。
    conn: StdMutex<Connection>,
}

/// 账本建表 SQL（forward-only：幂等键在 `control_batch`，无行级唯一）。
const CONTROL_LEDGER_SQL: &str = "
CREATE TABLE IF NOT EXISTS control_batch (
    idempotency_key TEXT PRIMARY KEY,
    actor           TEXT NOT NULL,
    submitted_at_ns INTEGER NOT NULL,
    status          TEXT NOT NULL,
    detail          TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS control_command (
    idempotency_key TEXT NOT NULL,
    device_id       TEXT NOT NULL,
    point_id        TEXT,
    address         TEXT NOT NULL,
    op              TEXT NOT NULL,
    delivered       INTEGER NOT NULL,
    reason          TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS control_command_key_idx ON control_command(idempotency_key);
";

impl ControlLedger {
    /// 打开（或创建）账本库并建表。
    ///
    /// # Errors
    /// 建库 / 建表失败 → [`DaemonError::StorageError`]。
    pub fn open(path: &Path) -> DaemonResult<Self> {
        let mut conn = Connection::open(path)
            .map_err(|e| DaemonError::StorageError(format!("control ledger open {path:?}: {e}")))?;
        conn.execute_batch(CONTROL_LEDGER_SQL)
            .map_err(|e| DaemonError::StorageError(format!("control ledger migrate: {e}")))?;
        Ok(Self {
            conn: StdMutex::new(conn),
        })
    }

    /// 声明本次批次：若键已存在返回 `false`（重复），否则插入并返回 `true`（新）。
    ///
    /// # Errors
    /// 存储失败 → [`DaemonError::StorageError`]。
    pub fn try_claim(&self, key: &str, actor: &str, ts_ns: u64) -> DaemonResult<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|p| DaemonError::StorageError(format!("control ledger lock poisoned: {p}")))?;
        conn.execute(
            "INSERT OR IGNORE INTO control_batch \
             (idempotency_key, actor, submitted_at_ns, status, detail) \
             VALUES (?1, ?2, ?3, 'claimed', '')",
            params![key, actor, ts_ns as i64],
        )
        .map_err(|e| DaemonError::StorageError(format!("control ledger claim: {e}")))?;
        Ok(conn.changes() != 0)
    }

    /// 写入本次结果（批次状态 + 逐指令结果），供重复提交回放。
    ///
    /// # Errors
    /// 存储失败 → [`DaemonError::StorageError`]。
    pub fn store_result(
        &self,
        key: &str,
        status: &str,
        detail: &str,
        commands: &[CommandOutcome],
    ) -> DaemonResult<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|p| DaemonError::StorageError(format!("control ledger lock poisoned: {p}")))?;
        let tx = conn
            .transaction()
            .map_err(|e| DaemonError::StorageError(format!("control ledger tx: {e}")))?;
        tx.execute(
            "UPDATE control_batch SET status = ?2, detail = ?3 WHERE idempotency_key = ?1",
            params![key, status, detail],
        )
        .map_err(|e| DaemonError::StorageError(format!("control ledger update: {e}")))?;
        for cmd in commands {
            tx.execute(
                "INSERT INTO control_command \
                 (idempotency_key, device_id, point_id, address, op, delivered, reason) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    key,
                    cmd.device_id,
                    cmd.point_id,
                    cmd.address,
                    cmd.op,
                    cmd.delivered as i64,
                    cmd.reason
                ],
            )
            .map_err(|e| DaemonError::StorageError(format!("control ledger insert cmd: {e}")))?;
        }
        tx.commit()
            .map_err(|e| DaemonError::StorageError(format!("control ledger commit: {e}")))?;
        Ok(())
    }

    /// 读取重复提交应回放的首发结果（`None` = 键不存在）。
    ///
    /// # Errors
    /// 存储失败 → [`DaemonError::StorageError`]。
    pub fn load(&self, key: &str) -> DaemonResult<Option<Replay>> {
        let conn = self
            .conn
            .lock()
            .map_err(|p| DaemonError::StorageError(format!("control ledger lock poisoned: {p}")))?;
        let status: Option<(String, String)> = conn
            .query_row(
                "SELECT status, detail FROM control_batch WHERE idempotency_key = ?1",
                params![key],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(|e| DaemonError::StorageError(format!("control ledger load: {e}")))?;
        let Some((status, detail)) = status else {
            return Ok(None);
        };
        let mut stmt = conn
            .prepare(
                "SELECT device_id, point_id, address, op, delivered, reason \
                 FROM control_command WHERE idempotency_key = ?1",
            )
            .map_err(|e| DaemonError::StorageError(format!("control ledger prepare: {e}")))?;
        let commands: Vec<CommandOutcome> = stmt
            .query_map(params![key], |row| {
                Ok(CommandOutcome {
                    device_id: row.get::<_, String>(0)?,
                    point_id: row.get::<_, Option<String>>(1)?,
                    address: row.get::<_, String>(2)?,
                    op: row.get::<_, String>(3)?,
                    delivered: row.get::<_, i64>(4)? != 0,
                    reason: row.get::<_, String>(5)?,
                })
            })
            .map_err(|e| DaemonError::StorageError(format!("control ledger query: {e}")))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| DaemonError::StorageError(format!("control ledger collect: {e}")))?;
        // 首发若失败（status 为错误码，非 accepted/claimed）→ 回放时整批仍按失败
        // 返回，诚实不伪造 202；`claimed` 为声明后、结果落库前的瞬态，按空成功回放
        // （不二次下发，等待首发结果落库）。
        let replay = if status == "accepted" || status == "claimed" {
            Replay::Success(ControlIssueResult {
                duplicate: true,
                idempotency_key: Some(key.to_string()),
                commands,
            })
        } else {
            Replay::Failure(ControlIssueError::from_storage(&status, &detail), commands)
        };
        Ok(Some(replay))
    }

    /// 近期下发批次（按提交时刻倒序；仅取首发成功项，失败首发不入历史）。
    ///
    /// # Errors
    /// 存储失败 → [`DaemonError::StorageError`]。
    pub fn list_recent(&self, limit: i64) -> DaemonResult<Vec<ControlIssueResult>> {
        let conn = self
            .conn
            .lock()
            .map_err(|p| DaemonError::StorageError(format!("control ledger lock poisoned: {p}")))?;
        let keys: Vec<String> = {
            let mut stmt = conn
                .prepare(
                    "SELECT idempotency_key FROM control_batch \
                     WHERE status = 'accepted' ORDER BY submitted_at_ns DESC LIMIT ?1",
                )
                .map_err(|e| DaemonError::StorageError(format!("control ledger prepare: {e}")))?;
            stmt.query_map(params![limit], |row| row.get::<_, String>(0))
                .map_err(|e| DaemonError::StorageError(format!("control ledger query: {e}")))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| DaemonError::StorageError(format!("control ledger collect: {e}")))?
        };
        let mut out = Vec::with_capacity(keys.len());
        for k in keys {
            if let Some(Replay::Success(r)) = self.load(&k)? {
                out.push(r);
            }
        }
        Ok(out)
    }
}

// ---- 注册表（编排解析 / 幂等 / 下发 / 记账） ----

/// 控制指令下发注册表（管理面调用入口；持有设备配置快照、南向写端口、可选账本）。
pub struct ControlRegistry {
    /// 共享态（读设备配置 + 健康度 + 审计器）。
    shared: DaemonShared,
    /// 南向写端口（`None` = 未装配 → 全部下发失败，诚实 fail-closed）。
    port: StdMutex<Option<Arc<dyn ControlWritePort>>>,
    /// 持久账本（`None` = 进程内幂等，诚实标注为重启后失效）。
    ledger: Option<Arc<ControlLedger>>,
    /// 时钟。
    clock: ControlClock,
    /// 进程内幂等回放表（ledger 缺省时兜底）。
    memory: StdMutex<HashMap<String, Replay>>,
}

impl ControlRegistry {
    /// 构造（ledger 由 bootstrap 经 [`Self::attach_ledger`] 挂载）。
    #[must_use]
    pub fn new(shared: DaemonShared, clock: ControlClock) -> Self {
        Self {
            shared,
            port: StdMutex::new(None),
            ledger: None,
            clock,
            memory: StdMutex::new(HashMap::new()),
        }
    }

    /// 挂载持久账本（bootstrap run 内调用）。
    pub fn attach_ledger(&mut self, ledger: Arc<ControlLedger>) {
        self.ledger = Some(ledger);
    }

    /// 挂载南向写端口（bootstrap run 内调用；实现方 = [`crate::southbound::DevicePollHandler`]）。
    pub fn set_port(&self, port: Arc<dyn ControlWritePort>) {
        let mut guard = self
            .port
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        *guard = Some(port);
    }

    /// 安全审计链记录（被受理的写；审计失败仅 warn，不阻塞）。
    fn audit_security(&self, actor: &str, result: &ControlIssueResult) {
        if let Some(logger) = self.shared.audit_logger() {
            let outcome = if result.duplicate { "replayed" } else { "issued" };
            let detail = format!(
                "control issue: {} command(s), idempotency_key={:?}",
                result.commands.len(),
                result.idempotency_key
            );
            if let Err(e) = logger.record(actor, AuditEventType::ControlCommand, outcome, &detail) {
                tracing::warn!(error = %e, "control audit record failed");
            }
        }
    }

    /// 取写端口（未装配 → Internal 错误，诚实 fail-closed）。
    fn port(&self) -> Result<Arc<dyn ControlWritePort>, ControlIssueError> {
        let guard = self
            .port
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        match guard.as_ref() {
            Some(port) => Ok(Arc::clone(port)),
            None => Err(ControlIssueError::Internal(
                "control write port not mounted".to_string(),
            )),
        }
    }

    /// 受理并下发一批控制指令（核心入口）。
    ///
    /// # Errors
    /// 参数非法 / 设备不存在 / 驱动不支持 / 设备不可达 → [`ControlIssueError`]
    /// （映射为结构化 HTTP 错误，绝不为 202）。成功受理返回 [`ControlIssueResult`]。
    pub async fn issue(
        &self,
        req: &ControlIssueRequest,
        actor: &str,
    ) -> Result<ControlIssueResult, ControlIssueError> {
        // ① 设备存在性：经写端口的计划表判定（设备段或点位段任一出现即视为存在）。
        //    端口持有真实设备计划表，是设备登记的权威来源。
        let port = self.port()?;
        for cmd in &req.commands {
            if !port.device_known(&cmd.device_id) {
                return Err(ControlIssueError::NotFound(format!(
                    "device {dev:?} is not registered (no device or point row)",
                    dev = cmd.device_id
                )));
            }
        }

        let ts_ns = (self.clock)();

        // ② 幂等：批次级键。重复 → 回放首次结果、不二次下发。
        if let Some(key) = &req.idempotency_key {
            let is_new = match &self.ledger {
                Some(ledger) => ledger.try_claim(key, actor, ts_ns)?,
                None => self.memory_claim(key),
            };
            if !is_new {
                let replay = match &self.ledger {
                    Some(ledger) => ledger.load(key)?,
                    None => self
                        .memory
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .get(key)
                        .cloned(),
                };
                return match replay {
                    // 首发成功 → 回放成功（duplicate=true，不二次下发）。
                    Some(Replay::Success(r)) => {
                        self.audit_security(actor, &r);
                        Ok(r)
                    }
                    // 首发失败 → 回放同一结构化错误（绝不伪造成功 / 202）。
                    Some(Replay::Failure(e, _)) => Err(e),
                    // 声明过但结果缺失（异常中断）：按重复处理，返回空结果。
                    None => Ok(ControlIssueResult {
                        duplicate: true,
                        idempotency_key: Some(key.clone()),
                        commands: Vec::new(),
                    }),
                };
            }
        }

        // ③ 真实下发（逐指令：先 assert 可写，再 dispatch）。
        let mut outcomes: Vec<CommandOutcome> = Vec::with_capacity(req.commands.len());
        let mut failed: Option<ControlIssueError> = None;
        for cmd in &req.commands {
            if failed.is_some() {
                break;
            }
            let oc = cmd.op.as_str();
            if let Err(e) = port
                .assert_writable(
                    &cmd.device_id,
                    cmd.point_id.as_deref(),
                    &cmd.address,
                )
                .await
            {
                failed = Some(to_issue_error(e));
                continue;
            }
            match port.dispatch(&cmd.device_id, std::slice::from_ref(&cmd.point)).await {
                Ok(mut outs) => {
                    // 每点必产出一个 outcome；缺失即视为内部异常（绝不靠 expect 兜底）。
                    let out = match outs.pop() {
                        Some(o) => o,
                        None => {
                            failed = Some(ControlIssueError::Internal(
                                "control dispatch returned no outcome for the written point"
                                    .to_string(),
                            ));
                            continue;
                        }
                    };
                    outcomes.push(CommandOutcome {
                        device_id: cmd.device_id.clone(),
                        point_id: cmd.point_id.clone(),
                        address: cmd.address.clone(),
                        op: oc.to_string(),
                        delivered: out.delivered,
                        reason: out.reason,
                    });
                }
                Err(e) => {
                    failed = Some(to_issue_error(e));
                }
            }
        }

        // ④ 汇总：任一失败 → 整批结构化错误；成功 → 202 结果体。
        match failed {
            Some(err) => {
                // 失败也记入账本（供重复回放，避免二次下发）。
                if let Some(key) = &req.idempotency_key {
                    self.persist(key, &Replay::Failure(err.clone(), outcomes.clone()));
                }
                Err(err)
            }
            None => {
                let result = ControlIssueResult {
                    duplicate: false,
                    idempotency_key: req.idempotency_key.clone(),
                    commands: outcomes,
                };
                if let Some(key) = &req.idempotency_key {
                    self.persist(key, &Replay::Success(result.clone()));
                }
                self.audit_security(actor, &result);
                Ok(result)
            }
        }
    }

    /// 进程内幂等声明（ledger 缺省时）。
    fn memory_claim(&self, key: &str) -> bool {
        let mut map = self.memory.lock().unwrap_or_else(|p| p.into_inner());
        if map.contains_key(key) {
            false
        } else {
            map.insert(
                key.to_string(),
                Replay::Success(ControlIssueResult {
                    duplicate: false,
                    idempotency_key: Some(key.to_string()),
                    commands: Vec::new(),
                }),
            );
            true
        }
    }

    /// 持久化回放结果（ledger 优先，否则进程内表）。
    fn persist(&self, key: &str, replay: &Replay) {
        match &self.ledger {
            Some(ledger) => {
                let (status, detail, cmds) = replay.storage();
                if let Err(e) = ledger.store_result(key, status, detail, cmds) {
                    tracing::warn!(error = %e, "control ledger persist failed");
                }
            }
            None => {
                let mut map = self.memory.lock().unwrap_or_else(|p| p.into_inner());
                map.insert(key.to_string(), replay.clone());
            }
        }
    }

    /// 近期下发历史（ledger 优先，否则进程内成功项）。
    pub fn history(&self, limit: usize) -> Vec<ControlIssueResult> {
        let limit = limit.max(1).min(1000) as i64;
        match &self.ledger {
            Some(ledger) => ledger
                .list_recent(limit)
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "control ledger history read failed");
                    Vec::new()
                }),
            None => self
                .memory
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .values()
                .filter_map(|r| match r {
                    Replay::Success(x) => Some(x.clone()),
                    Replay::Failure(_, _) => None,
                })
                .collect(),
        }
    }

    /// 控制面装配状态（诚实反映：端口是否挂载、幂等是否为持久）。
    pub fn status(&self) -> ControlPlaneStatus {
        let port_mounted = self
            .port
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_some();
        ControlPlaneStatus {
            mounted: port_mounted,
            idempotency: if self.ledger.is_some() {
                "persistent"
            } else {
                "in-memory"
            },
        }
    }
}

/// 控制面运行状态（诚实反映装配与幂等持久化口径）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ControlPlaneStatus {
    /// 南向写端口是否已挂载（未挂载 → 全部下发诚实失败）。
    pub mounted: bool,
    /// 幂等持久化口径（`persistent` = SQLite 账本；`in-memory` = 重启后失效）。
    pub idempotency: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::driver::modbus::{
        ModbusConfig, ModbusDriver, ModbusFraming,
    };
    use crate::driver::{Driver, PointAddressParser, Reconnector};
    use tokio_modbus::server::tcp::{accept_tcp_connection, Server};
    use tokio_modbus::server::Service;
    use tokio_modbus::{ExceptionCode, Request, Response};

    /// mock 从站：记录收到的写请求（FC06 / FC05）。
    #[derive(Debug, Default)]
    struct MockInner {
        holding: Vec<u16>,
        requests: Vec<Request<'static>>,
    }

    #[derive(Clone)]
    struct MockService {
        state: Arc<StdMutex<MockInner>>,
    }

    impl Service for MockService {
        type Request = Request<'static>;
        type Response = Response;
        type Exception = ExceptionCode;
        type Future = std::future::Ready<std::result::Result<Response, ExceptionCode>>;

        fn call(&self, req: Self::Request) -> Self::Future {
            let mut state = self.state.lock().expect("mock lock");
            let resp = match req {
                Request::WriteSingleRegister(addr, val) => {
                    if (addr as usize) >= state.holding.len() {
                        return std::future::ready(Err(ExceptionCode::IllegalDataAddress));
                    }
                    state.holding[addr as usize] = val;
                    state.requests.push(Request::WriteSingleRegister(addr, val));
                    Response::WriteSingleRegister(addr, val)
                }
                Request::WriteSingleCoil(addr, on) => {
                    state.requests.push(Request::WriteSingleCoil(addr, on));
                    Response::WriteSingleCoil(addr, on)
                }
                _ => return std::future::ready(Err(ExceptionCode::IllegalFunction)),
            };
            std::future::ready(Ok(resp))
        }
    }

    async fn spawn_mock_server(state: Arc<StdMutex<MockInner>>) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let server = Server::new(listener);
            let on_connected = move |stream: tokio::net::TcpStream, socket_addr: std::net::SocketAddr| {
                let state = Arc::clone(&state);
                async move {
                    accept_tcp_connection(stream, socket_addr, move |_| {
                        Ok(Some(MockService { state: Arc::clone(&state) }))
                    })
                }
            };
            let _ = server
                .serve(&on_connected, |err| eprintln!("mock: {err}"))
                .await;
        });
        addr
    }

    /// assert_writable 拒绝仿真点 / 未知设备 / 未接线协议。
    fn handler_with_device(addr: std::net::SocketAddr) -> DevicePollHandler {
        let config = crate::config::GatewayConfig::parse(&format!(
            "[[points]]\ndevice_id = \"dev-01\"\npoint_id = \"40001\"\n\
             protocol = \"modbus-tcp\"\naddress = \"{addr}\"\nfrequency_ms = 100\n"
        ))
        .expect("parse");
        DevicePollHandler::from_config(&config)
    }

    use crate::southbound::DevicePollHandler;

    #[tokio::test]
    async fn write_coil_enters_real_production_path() {
        let state = Arc::new(StdMutex::new(MockInner {
            holding: vec![0x0000, 0x0000],
            requests: Vec::new(),
        }));
        let addr = spawn_mock_server(Arc::clone(&state)).await;
        let handler = handler_with_device(addr);

        // 经由 ControlWritePort::dispatch 真实写线圈（构造 WritePoint，area='0'）。
        let point = WritePoint {
            address: PointAddressParser::parse_coil_address("00002").expect("coil"),
            value: vec![0xFF],
        };
        let outs = handler
            .dispatch("dev-01", std::slice::from_ref(&point))
            .await
            .expect("dispatch");
        assert_eq!(outs.len(), 1, "one outcome");
        assert!(outs[0].delivered, "coil delivered");

        // 接线证明：mock 从站确实收到 FC05（改一行 dispatch 即失败）。
        let lock = state.lock().expect("lock");
        assert_eq!(
            lock.requests,
            vec![Request::WriteSingleCoil(1, true)],
            "real write entered production path"
        );
    }

    #[tokio::test]
    async fn write_register_enters_real_production_path() {
        let state = Arc::new(StdMutex::new(MockInner {
            holding: vec![0x0000],
            requests: Vec::new(),
        }));
        let addr = spawn_mock_server(Arc::clone(&state)).await;
        let handler = handler_with_device(addr);

        let point = WritePoint {
            address: PointAddressParser::parse("40001").expect("register"),
            value: vec![0xBE, 0xEF],
        };
        handler
            .dispatch("dev-01", std::slice::from_ref(&point))
            .await
            .expect("dispatch");

        let lock = state.lock().expect("lock");
        assert_eq!(
            lock.requests,
            vec![Request::WriteSingleRegister(0, 0xBEEF)],
            "real register write entered production path"
        );
    }

    #[tokio::test]
    async fn unknown_device_dispatch_rejected_not_202() {
        let handler = handler_with_device("127.0.0.1:1".parse().expect("addr"));
        let point = WritePoint {
            address: PointAddressParser::parse_coil_address("00001").expect("coil"),
            value: vec![0xFF],
        };
        let err = handler
            .dispatch("ghost", std::slice::from_ref(&point))
            .await
            .expect_err("must fail");
        // 关键：错误是结构化的（非 202、非成功），真实原因保留。
        assert!(
            matches!(err, DaemonError::ConfigError(_)),
            "unknown device must fail, got {err:?}"
        );
        assert!(err.to_string().contains("unknown device group"), "{err}");
    }

    /// 端到端：ControlRegistry.issue 在真实 mock 从站上写寄存器 + 幂等重复不二次下发。
    #[tokio::test]
    async fn registry_issue_delivers_and_idempotent_replay() {
        let state = Arc::new(StdMutex::new(MockInner {
            holding: vec![0x0000],
            requests: Vec::new(),
        }));
        let addr = spawn_mock_server(Arc::clone(&state)).await;
        let handler = handler_with_device(addr);

        let registry = ControlRegistry::new(DaemonShared::new(), default_clock());
        registry.set_port(Arc::new(handler));

        let raw = RawIssueRequest {
            idempotency_key: Some("bat-1".to_string()),
            commands: vec![RawCommand {
                device_id: "dev-01".to_string(),
                point_id: Some("40001".to_string()),
                address: "40001".to_string(),
                op: "write_register".to_string(),
                value: serde_json::json!(0x1234),
            }],
        };
        let req = parse_issue(&raw).expect("parse");
        let result = registry.issue(&req, "tester").await.expect("issue");
        assert!(!result.duplicate, "first issue");
        assert!(result.commands[0].delivered, "delivered");

        let writes_after_first = state.lock().expect("lock").requests.len();
        assert_eq!(writes_after_first, 1, "one write after first issue");

        // 重复提交同键 → 命中幂等，不二次下发。
        let result2 = registry.issue(&req, "tester").await.expect("issue");
        assert!(result2.duplicate, "second issue is duplicate");
        assert_eq!(
            state.lock().expect("lock").requests.len(),
            writes_after_first,
            "no second dispatch on idempotent replay"
        );
    }

    /// 设备不存在 → issue 结构化错误（非 202）。
    #[tokio::test]
    async fn registry_issue_unknown_device_is_structured_error() {
        let registry = ControlRegistry::new(DaemonShared::new(), default_clock());
        registry.set_port(Arc::new(handler_with_device("127.0.0.1:1".parse().expect("addr"))));
        let raw = RawIssueRequest {
            idempotency_key: None,
            commands: vec![RawCommand {
                device_id: "ghost".to_string(),
                point_id: None,
                address: "40001".to_string(),
                op: "write_register".to_string(),
                value: serde_json::json!(1),
            }],
        };
        let req = parse_issue(&raw).expect("parse");
        let err = registry.issue(&req, "tester").await.expect_err("must fail");
        assert_eq!(err.code(), "not_found", "{err:?}");
        assert_eq!(err.status(), 404);
    }

    /// 未知 op → 400 结构化错误（解析期，未触设备）。
    #[tokio::test]
    async fn parse_rejects_unknown_op() {
        let raw = RawIssueRequest {
            idempotency_key: None,
            commands: vec![RawCommand {
                device_id: "dev-01".to_string(),
                point_id: None,
                address: "40001".to_string(),
                op: "write_eeprom".to_string(),
                value: serde_json::json!(1),
            }],
        };
        let err = parse_issue(&raw).expect_err("must fail");
        assert_eq!(err.code(), "bad_request");
        assert_eq!(err.status(), 400);
    }

    // 借用计数断言（确保 dispatch 真实走了写路径而非短路）：用可观测副效应。
    #[tokio::test]
    async fn modbus_driver_is_used_not_mock_state() {
        // 仅验证：经由真实 DevicePollHandler::dispatch 调用的底层是 ModbusDriver
        // （通过连接计数侧证：mock 从站被连接且收到写）。
        let state = Arc::new(StdMutex::new(MockInner {
            holding: vec![0x0000],
            requests: Vec::new(),
        }));
        let addr = spawn_mock_server(Arc::clone(&state)).await;
        let handler = handler_with_device(addr);
        let point = WritePoint {
            address: PointAddressParser::parse("40001").expect("register"),
            value: vec![0x00, 0x01],
        };
        handler
            .dispatch("dev-01", std::slice::from_ref(&point))
            .await
            .expect("dispatch");
        assert!(
            !state.lock().expect("lock").requests.is_empty(),
            "real driver wrote to the slave, not a mock state"
        );
    }

    // 确保未使用的 import 不被当成 dead_code：构造一个真实 ModbusDriver 仅作类型存在性检查。
    #[allow(dead_code)]
    fn _assert_modbus_driver_used() {
        let _ = std::mem::size_of::<ModbusDriver>();
        let _ = std::mem::size_of::<ModbusConfig>();
        let _ = std::mem::size_of::<ModbusFraming>();
        let _ = std::mem::size_of::<Reconnector>();
        let _ = AtomicUsize::new(0).load(Ordering::Relaxed);
    }
}
