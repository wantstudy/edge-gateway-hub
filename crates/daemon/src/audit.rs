//! task 26 — 安全审计模块（`AuditLogger`：追加写入 + 防篡改哈希链 + SQLite 落盘）。
//!
//! ## 职责边界
//! - **做**：安全审计事件的**持久**记录（登录 / 登录失败 / 配置修改 / 授权失败 /
//!   试用期到期 / 审计拉取自身），每条事件进入 SQLite `audit_log` 表并以
//!   **HMAC-SHA256 哈希链**串联（`entry_hash = HMAC(K, seq‖ts‖actor‖event‖
//!   outcome‖detail‖prev_hash)`）——任何一行被改 / 删 / 插入都会使
//!   [`AuditLogger::verify_chain`] 定位到首个断裂点。表结构经
//!   `migrations::audit_registry()`（task 55 增量迁移框架，v2）创建，并以
//!   **数据库触发器**强制追加写（UPDATE / DELETE 一律 `RAISE(ABORT)`，
//!   QA 场景「尝试删除日志 → 断言失败」的落点）。
//! - **不做**：审计日志前端（task 30）；北向发送审计（`backpressure::AuditLog`，
//!   另一回事）；mgmt HTTP 面在 `mgmt::audit_api`（本模块只提供库能力）。
//!
//! ## 防篡改密钥派生（不硬编码）
//! - `K = HKDF-Expand(HKDF-Extract(salt, IKM), "iot-daq/audit-chain/v1")`
//!   （RFC 5869 手工实现，与 `telemetry_store` 同法；`hkdf` crate 不在依赖树）；
//! - `salt`：每安装随机 32 hex（uuid v4 ×2），存 `audit_meta` 表（重开可复算）；
//! - `IKM`：部署方注入（[`resolve_audit_ikm`]：env `IOT_DAQ_AUDIT_SECRET` 优先，
//!   回退授权机器码）。**密钥常量不进代码**；IKM 缺失时降级为
//!   `IKM = salt`（`open` 时 warn 显式声明：防改写能力弱于绑定部署密钥形态，
//!   仍可检测不重算链的普通篡改）。
//!
//! ## 实现红线
//! - 零 panic：锁中毒 `into_inner` 恢复；错误收敛 `DaemonError::StorageError`；
//! - **大数红线**：JSON 里 `seq` / `ts_ns` 一律**字符串编码**（手工 `json!`）；
//! - 写连接只 INSERT，从不 UPDATE / DELETE（追加写语义的代码侧约束）。

use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use sha2::Sha256;

use crate::error::{DaemonError, DaemonResult};

/// 审计库文件名（固定；与 telemetry.db / queue.db 物理隔离，独立库文件）。
pub const AUDIT_DB_FILE_NAME: &str = "audit.db";
/// 部署审计 IKM 环境变量（hex 优先解码；非 hex 按原始字节）。
pub const AUDIT_SECRET_ENV: &str = "IOT_DAQ_AUDIT_SECRET";
/// 哈希链创始块的 prev_hash（64 个 '0'，即 SHA-256 十六进制宽度的零串）。
pub const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";
/// HKDF-Expand 的 info 域（域分离，防与其他用途的派生密钥混用）。
const CHAIN_INFO: &[u8] = b"iot-daq/audit-chain/v1";
/// 单条审计记录 detail 的最大字符数（防御性截断，防单行膨胀）。
const MAX_DETAIL_CHARS: usize = 2000;
/// actor 的最大字符数。
const MAX_ACTOR_CHARS: usize = 256;
/// 单次查询返回上限（防御性，防全表倾泻）。
pub const MAX_QUERY_LIMIT: u32 = 1000;
/// 查询缺省 limit。
pub const DEFAULT_QUERY_LIMIT: u32 = 100;

// ---- 审计结果字面量（与 remote_ops 的 OUTCOME_* 同词表） ----

/// 动作被受理。
pub const OUTCOME_ACCEPTED: &str = "accepted";
/// 被鉴权拒绝。
pub const OUTCOME_DENIED: &str = "denied";
/// 请求参数非法。
pub const OUTCOME_BAD_REQUEST: &str = "bad_request";
/// 受理后执行失败（落盘 / IO 等）。
pub const OUTCOME_FAILED: &str = "failed";

// ---- 事件类型 ----

/// 安全审计事件类型（计划 task 26 事件域 + 审计拉取自身）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditEventType {
    /// 登录成功。
    Login,
    /// 登录失败（含请求体非法；detail 不区分原因——防账号枚举）。
    LoginFailed,
    /// 配置修改（设备 / 点位写接口受理）。
    ConfigChange,
    /// 授权失败（RBAC 拒绝等鉴权后拒绝事件）。
    AuthzFailed,
    /// 试用期到期（授权状态机迁移；license 侧接线随其文件域推进）。
    TrialExpired,
    /// 审计日志在线查询（拉取行为自身入链，可追溯「谁看过审计」）。
    AuditRead,
    /// 审计日志导出（数据出境动作，`audit.export` 仅 system 可授）。
    AuditExport,
}

impl AuditEventType {
    /// 事件类型字面量（小写下划线；持久化与 HTTP 过滤共用）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            AuditEventType::Login => "login",
            AuditEventType::LoginFailed => "login_failed",
            AuditEventType::ConfigChange => "config_change",
            AuditEventType::AuthzFailed => "authz_failed",
            AuditEventType::TrialExpired => "trial_expired",
            AuditEventType::AuditRead => "audit_read",
            AuditEventType::AuditExport => "audit_export",
        }
    }

    /// 解析事件类型字面量（未知值 → `None`，供 HTTP 过滤参数校验）。
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "login" => Some(AuditEventType::Login),
            "login_failed" => Some(AuditEventType::LoginFailed),
            "config_change" => Some(AuditEventType::ConfigChange),
            "authz_failed" => Some(AuditEventType::AuthzFailed),
            "trial_expired" => Some(AuditEventType::TrialExpired),
            "audit_read" => Some(AuditEventType::AuditRead),
            "audit_export" => Some(AuditEventType::AuditExport),
            _ => None,
        }
    }
}

// ---- 记录与查询 ----

/// 单条持久安全审计记录（哈希链的一个节点）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRecord {
    /// 链内序号（从 1 单调递增；JSON 编码为字符串，大数红线）。
    pub seq: u64,
    /// 记录时刻（UTC 纳秒，来自注入时钟；JSON 编码为字符串）。
    pub ts_ns: u64,
    /// 操作者（登录事件 = 用户名；RBAC 拒绝 = JWT sub）。
    pub actor: String,
    /// 事件类型字面量（[`AuditEventType::as_str`]）。
    pub event: String,
    /// 结果字面量（accepted / denied / bad_request / failed）。
    pub outcome: String,
    /// 详情（人读；不含凭据与敏感值）。
    pub detail: String,
    /// 前一条记录的 entry_hash（创始块为 [`GENESIS_HASH`]）。
    pub prev_hash: String,
    /// 本条记录的链哈希（HMAC-SHA256 hex）。
    pub entry_hash: String,
}

impl AuditRecord {
    /// 序列化为 JSON（`seq` / `ts_ns` 一律字符串，大数红线）。
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "seq": self.seq.to_string(),
            "ts_ns": self.ts_ns.to_string(),
            "actor": self.actor,
            "event": self.event,
            "outcome": self.outcome,
            "detail": self.detail,
            "prev_hash": self.prev_hash,
            "entry_hash": self.entry_hash,
        })
    }
}

/// 审计查询条件（分页 + 时间窗 + 事件类型过滤）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditQuery {
    /// 返回条数上限（> 0，超过 [`MAX_QUERY_LIMIT`] 时被钳制）。
    pub limit: u32,
    /// 跳过条数（分页偏移）。
    pub offset: u64,
    /// 时间下界（UTC 纳秒，闭区间；`None` = 不限）。
    pub since_ns: Option<u64>,
    /// 时间上界（UTC 纳秒，闭区间；`None` = 不限）。
    pub until_ns: Option<u64>,
    /// 事件类型过滤（精确匹配 [`AuditEventType::as_str`]；`None` = 全类型）。
    pub event: Option<String>,
}

impl Default for AuditQuery {
    fn default() -> Self {
        Self {
            limit: DEFAULT_QUERY_LIMIT,
            offset: 0,
            since_ns: None,
            until_ns: None,
            event: None,
        }
    }
}

impl AuditQuery {
    /// 缺省查询（limit = [`DEFAULT_QUERY_LIMIT`]，无过滤）。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 设置返回上限（0 视为非法，回退缺省值）。
    #[must_use]
    pub fn with_limit(mut self, limit: u32) -> Self {
        self.limit = if limit == 0 {
            DEFAULT_QUERY_LIMIT
        } else {
            limit
        };
        self
    }

    /// 设置分页偏移。
    #[must_use]
    pub fn with_offset(mut self, offset: u64) -> Self {
        self.offset = offset;
        self
    }

    /// 设置时间下界（UTC 纳秒，闭区间）。
    #[must_use]
    pub fn with_since_ns(mut self, since_ns: u64) -> Self {
        self.since_ns = Some(since_ns);
        self
    }

    /// 设置时间上界（UTC 纳秒，闭区间）。
    #[must_use]
    pub fn with_until_ns(mut self, until_ns: u64) -> Self {
        self.until_ns = Some(until_ns);
        self
    }

    /// 设置事件类型过滤（精确字面量；未知字面量在查询层返回空集，HTTP 层应先行校验）。
    #[must_use]
    pub fn with_event(mut self, event: impl Into<String>) -> Self {
        self.event = Some(event.into());
        self
    }
}

/// 哈希链校验报告（[`AuditLogger::verify_chain`] 的输出）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainVerifyReport {
    /// 表内总行数。
    pub total: u64,
    /// 通过校验的连续前缀长度。
    pub verified: u64,
    /// 整链是否完好（seq 连续 + prev_hash 串联 + 每条 entry_hash 重算一致）。
    pub ok: bool,
    /// 首个断裂点的 seq（`ok == true` 时为 `None`）。
    pub first_broken_seq: Option<u64>,
}

// ---- 密码学原语（纯 Rust：hmac + sha2，零新增依赖） ----

/// HMAC-SHA256（`hmac` 对任意长度密钥均有效；`new_from_slice` 的 Err 分支
/// 理论不可达，按零 panic 红线回退为空密钥并退化为常量输出——仅影响该次
/// 理论不可达的调用，绝不 panic）。
fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = match <Hmac<Sha256> as Mac>::new_from_slice(key) {
        Ok(mac) => mac,
        Err(_) => match <Hmac<Sha256> as Mac>::new_from_slice(&[]) {
            Ok(mac) => mac,
            // 双重不可达：空密钥对 HMAC 恒合法。退化为全零输出而非 panic。
            Err(_) => return [0u8; 32],
        },
    };
    mac.update(data);
    let out = mac.finalize().into_bytes();
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&out);
    digest
}

/// HKDF-Extract（RFC 5869 §2.2）：`PRK = HMAC-SHA256(salt, IKM)`。
/// `hkdf` crate 不在依赖树，按 `telemetry_store` 先例手工实现。
fn hkdf_extract(salt: &[u8], ikm: &[u8]) -> [u8; 32] {
    hmac_sha256(salt, ikm)
}

/// HKDF-Expand 单块（RFC 5869 §2.3）：`OKM = HMAC-SHA256(PRK, info || 0x01)`。
/// 32 字节输出恰好一个块，无需迭代。
fn hkdf_expand_single(prk: &[u8; 32], info: &[u8]) -> [u8; 32] {
    let mut data = Vec::with_capacity(info.len() + 1);
    data.extend_from_slice(info);
    data.push(0x01);
    hmac_sha256(prk, &data)
}

/// 恒时比较（hex 字符串；长度不等直接 `false`——长度本身不构成秘密）。
fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 审计链密钥派生：`K = HKDF-Expand(HKDF-Extract(salt, IKM), CHAIN_INFO)`。
///
/// IKM 缺失时以盐自派生（降级形态，见 [`AuditLogger::open`] 注释）。
fn derive_chain_key(salt: &[u8], ikm: Option<&[u8]>) -> [u8; 32] {
    let ikm_bytes: &[u8] = match ikm {
        Some(ikm) if !ikm.is_empty() => ikm,
        _ => salt,
    };
    let prk = hkdf_extract(salt, ikm_bytes);
    hkdf_expand_single(&prk, CHAIN_INFO)
}

/// 计算单条记录的链哈希（域内以 `\u{1f}` 分隔，固定字段顺序）。
#[allow(clippy::too_many_arguments)]
fn compute_entry_hash(
    key: &[u8; 32],
    seq: u64,
    ts_ns: u64,
    actor: &str,
    event: &str,
    outcome: &str,
    detail: &str,
    prev_hash: &str,
) -> String {
    let canonical = format!(
        "iot-daq/audit/v1\u{1f}{seq}\u{1f}{ts_ns}\u{1f}{actor}\u{1f}{event}\u{1f}{outcome}\u{1f}{detail}\u{1f}{prev_hash}"
    );
    hex::encode(hmac_sha256(key, canonical.as_bytes()))
}

// ---- IKM 解析（生产装配路径） ----

/// 解析审计链 IKM（部署密钥，**不硬编码**）：
///
/// 1. env [`AUDIT_SECRET_ENV`]（`IOT_DAQ_AUDIT_SECRET`）：非空即用——合法 hex
///    优先解码为字节，否则按原始 UTF-8 字节；
/// 2. 回退：授权机器码（`LicensingClient::machine_code()`，bootstrap 装配时注入）；
/// 3. 两者皆无 → `None`（`open` 降级为盐自派生密钥并 warn）。
#[must_use]
pub fn resolve_audit_ikm(machine_code: Option<&str>) -> Option<Vec<u8>> {
    if let Ok(secret) = std::env::var(AUDIT_SECRET_ENV) {
        let trimmed = secret.trim();
        if !trimmed.is_empty() {
            return Some(match hex::decode(trimmed) {
                Ok(bytes) if !bytes.is_empty() => bytes,
                _ => trimmed.as_bytes().to_vec(),
            });
        }
    }
    let machine = machine_code?.trim();
    if machine.is_empty() {
        return None;
    }
    Some(machine.as_bytes().to_vec())
}

// ---- AuditLogger ----

/// 持久安全审计记录器（SQLite + 追加写 + HMAC-SHA256 防篡改哈希链）。
///
/// 克隆廉价不需要：mgmt / bootstrap 以 `Arc<AuditLogger>` 共享；内部
/// `Mutex<Connection>` 串行化写（审计写入低频，无性能顾虑）。
pub struct AuditLogger {
    /// 独立 SQLite 写连接（只 INSERT / SELECT；UPDATE / DELETE 由触发器拒绝）。
    conn: Mutex<Connection>,
    /// 链密钥（HKDF 派生，见模块注释；不落盘、不打印）。
    key: [u8; 32],
    /// 时间源（UTC 纳秒；测试注入受控时钟保证确定性）。
    clock: Box<dyn Fn() -> u64 + Send + Sync>,
}

impl AuditLogger {
    /// 打开（或创建）审计库。
    ///
    /// 流程：开库 → WAL + FULL 同步 → 增量迁移（v1 账本 + v2 审计表，task 55
    /// 框架）→ 读取 / 生成链盐 → 派生链密钥。IKM 为 `None` / 空时降级为盐自
    /// 派生密钥并 `warn`（防篡改能力弱于绑定部署密钥；绝不因缺密钥而拒绝启动）。
    ///
    /// # Errors
    /// 开库 / 迁移 / 盐读写失败 → `DaemonError::StorageError`。
    pub fn open(db_path: &Path, ikm: Option<&[u8]>) -> DaemonResult<Self> {
        let conn = Connection::open(db_path)
            .map_err(|e| DaemonError::StorageError(format!("audit db open: {e}")))?;
        // FULL 同步：审计记录的落盘耐久性优先于写入吞吐（低频写路径）。
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
            .map_err(|e| DaemonError::StorageError(format!("audit db pragmas: {e}")))?;
        crate::migrations::run_migrations_with(&conn, &crate::migrations::audit_registry())?;

        let salt = ensure_chain_salt(&conn)?;
        if ikm.is_none_or(|ikm| ikm.is_empty()) {
            tracing::warn!(
                "audit: no deployment IKM ({AUDIT_SECRET_ENV} unset and no machine code bound); \
                 tamper-evidence degraded to per-install salt key"
            );
        }
        let key = derive_chain_key(&salt, ikm);
        Ok(Self {
            conn: Mutex::new(conn),
            key,
            clock: Box::new(system_clock_ns),
        })
    }

    /// 注入受控时钟（UTC 纳秒；测试确定性用；必须在共享前调用）。
    #[must_use]
    pub fn with_clock(mut self, clock: Box<dyn Fn() -> u64 + Send + Sync>) -> Self {
        self.clock = clock;
        self
    }

    /// 取写连接（毒锁恢复，零 panic）。
    fn lock_conn(&self) -> MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 追加一条审计记录（入哈希链；原子：读链尾 + INSERT 在同一锁与事务内）。
    ///
    /// # Errors
    /// SQLite 写失败 → `DaemonError::StorageError`（调用方决定告警策略；
    /// HTTP 面约定「审计失败不阻塞主流程，只 `tracing::warn`」）。
    pub fn record(
        &self,
        actor: &str,
        event: AuditEventType,
        outcome: &str,
        detail: &str,
    ) -> DaemonResult<AuditRecord> {
        // unchecked_transaction 仅需 &Connection（rusqlite 事务 API 的 &mut 形式
        // 不适用共享锁连接场景）。
        let conn = self.lock_conn();
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| DaemonError::StorageError(format!("audit tx: {e}")))?;
        let record = self.append_in_tx(&tx, actor, event, outcome, detail)?;
        tx.commit()
            .map_err(|e| DaemonError::StorageError(format!("audit commit: {e}")))?;
        Ok(record)
    }

    /// 事务内的追加实现（`record` 与迁移后的首条种子共用）。
    fn append_in_tx(
        &self,
        tx: &Connection,
        actor: &str,
        event: AuditEventType,
        outcome: &str,
        detail: &str,
    ) -> DaemonResult<AuditRecord> {
        // 链尾：当前最大 seq 及其 entry_hash（空表 = 创始块）。
        let (latest_seq, prev_hash): (u64, String) = {
            let result: std::result::Result<(i64, String), rusqlite::Error> = tx.query_row(
                "SELECT seq, entry_hash FROM audit_log ORDER BY seq DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            );
            match result {
                Ok((seq, hash)) => (u64::try_from(seq.max(0)).unwrap_or(u64::MAX), hash),
                Err(rusqlite::Error::QueryReturnedNoRows) => (0, GENESIS_HASH.to_string()),
                Err(e) => {
                    return Err(DaemonError::StorageError(format!("audit tail read: {e}")));
                }
            }
        };
        let seq = latest_seq
            .checked_add(1)
            .ok_or_else(|| DaemonError::StorageError("audit seq overflow".to_string()))?;
        let ts_ns = (self.clock)();
        let actor = clamp_str(actor, MAX_ACTOR_CHARS);
        let actor = if actor.is_empty() {
            "<missing>"
        } else {
            actor.as_str()
        };
        let detail = clamp_str(detail, MAX_DETAIL_CHARS);

        let entry_hash = compute_entry_hash(
            &self.key,
            seq,
            ts_ns,
            actor,
            event.as_str(),
            outcome,
            &detail,
            &prev_hash,
        );
        let ts_ns_i64 = i64::try_from(ts_ns).unwrap_or(i64::MAX);
        tx.execute(
            "INSERT INTO audit_log(seq, ts_ns, actor, event, outcome, detail, prev_hash, entry_hash) \
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                i64::try_from(seq).unwrap_or(i64::MAX),
                ts_ns_i64,
                actor,
                event.as_str(),
                outcome,
                detail,
                prev_hash,
                entry_hash,
            ],
        )
        .map_err(|e| DaemonError::StorageError(format!("audit insert: {e}")))?;

        Ok(AuditRecord {
            seq,
            ts_ns,
            actor: actor.to_string(),
            event: event.as_str().to_string(),
            outcome: outcome.to_string(),
            detail,
            prev_hash,
            entry_hash,
        })
    }

    /// 当前链尾序号（空链 = 0）。
    ///
    /// # Errors
    /// SQLite 读失败 → `DaemonError::StorageError`。
    pub fn latest_seq(&self) -> DaemonResult<u64> {
        let conn = self.lock_conn();
        let raw: Option<i64> = conn
            .query_row("SELECT MAX(seq) FROM audit_log", [], |row| row.get(0))
            .map_err(|e| DaemonError::StorageError(format!("audit max seq: {e}")))?;
        Ok(raw.map_or(0, |v| u64::try_from(v.max(0)).unwrap_or(u64::MAX)))
    }

    /// 按条件查询（保序：seq 升序；limit 被钳制到 [`MAX_QUERY_LIMIT`]）。
    ///
    /// # Errors
    /// SQLite 读失败 → `DaemonError::StorageError`。
    pub fn query(&self, query: &AuditQuery) -> DaemonResult<Vec<AuditRecord>> {
        let conn = self.lock_conn();
        let limit = query.limit.clamp(1, MAX_QUERY_LIMIT);
        let since = i64::try_from(query.since_ns.unwrap_or(0)).unwrap_or(0);
        let until = i64::try_from(query.until_ns.unwrap_or(u64::MAX)).unwrap_or(i64::MAX);
        let event_filter = query.event.clone().unwrap_or_default();
        let mut stmt = conn
            .prepare(
                "SELECT seq, ts_ns, actor, event, outcome, detail, prev_hash, entry_hash \
                 FROM audit_log \
                 WHERE ts_ns >= ?1 AND ts_ns <= ?2 AND (?3 = '' OR event = ?3) \
                 ORDER BY seq LIMIT ?4 OFFSET ?5",
            )
            .map_err(|e| DaemonError::StorageError(format!("audit query prepare: {e}")))?;
        let rows = stmt
            .query_map(
                params![
                    since,
                    until,
                    event_filter,
                    i64::from(limit),
                    i64::try_from(query.offset).unwrap_or(i64::MAX)
                ],
                row_to_record,
            )
            .map_err(|e| DaemonError::StorageError(format!("audit query: {e}")))?;
        let mut records = Vec::new();
        for row in rows {
            records.push(row.map_err(|e| DaemonError::StorageError(format!("audit row: {e}")))?);
        }
        Ok(records)
    }

    /// 校验整条哈希链（seq 连续 + prev_hash 串联 + 每条重算一致）。
    ///
    /// # Errors
    /// SQLite 读失败 → `DaemonError::StorageError`（链断裂**不是**错误，
    /// 以 `ChainVerifyReport.ok == false` 表达）。
    pub fn verify_chain(&self) -> DaemonResult<ChainVerifyReport> {
        let conn = self.lock_conn();
        let mut stmt = conn
            .prepare(
                "SELECT seq, ts_ns, actor, event, outcome, detail, prev_hash, entry_hash \
                 FROM audit_log ORDER BY seq",
            )
            .map_err(|e| DaemonError::StorageError(format!("audit verify prepare: {e}")))?;
        let rows = stmt
            .query_map([], row_to_record)
            .map_err(|e| DaemonError::StorageError(format!("audit verify: {e}")))?;

        let mut total = 0u64;
        let mut verified = 0u64;
        let mut first_broken_seq: Option<u64> = None;
        let mut prev_stored_hash = GENESIS_HASH.to_string();
        let mut expected_seq = 1u64;

        for row in rows {
            let record = row.map_err(|e| DaemonError::StorageError(format!("audit row: {e}")))?;
            total += 1;
            if first_broken_seq.is_some() {
                continue; // 已定位首断点：继续统计总行数即可。
            }
            // ① seq 必须严格连续（删行即断；断点定位在**缺失的序号**上——
            // 现存首行 seq > expected 说明 expected 被删除）。
            if record.seq != expected_seq {
                first_broken_seq = Some(expected_seq);
                continue;
            }
            // ② prev_hash 必须与上一条存储的 entry_hash 一致（改 prev_hash 即断）。
            if !ct_eq(&record.prev_hash, &prev_stored_hash) {
                first_broken_seq = Some(record.seq);
                continue;
            }
            // ③ entry_hash 必须与按存储字段重算的值一致（改任意字段即断）。
            let recomputed = compute_entry_hash(
                &self.key,
                record.seq,
                record.ts_ns,
                &record.actor,
                &record.event,
                &record.outcome,
                &record.detail,
                &record.prev_hash,
            );
            if !ct_eq(&recomputed, &record.entry_hash) {
                first_broken_seq = Some(record.seq);
                continue;
            }
            prev_stored_hash = record.entry_hash.clone();
            expected_seq += 1;
            verified += 1;
        }

        Ok(ChainVerifyReport {
            total,
            verified,
            ok: first_broken_seq.is_none(),
            first_broken_seq,
        })
    }
}

impl std::fmt::Debug for AuditLogger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 密钥与连接不进 Debug 输出。
        f.debug_struct("AuditLogger").finish_non_exhaustive()
    }
}

// ---- 内部辅助 ----

/// 当前 UTC 纳秒（时钟早于纪元按 0 处理，不 panic）。
fn system_clock_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
}

/// 按字符数截断（不产生半个 UTF-8 字符）。
fn clamp_str(raw: &str, max_chars: usize) -> String {
    if raw.chars().count() <= max_chars {
        raw.to_string()
    } else {
        raw.chars().take(max_chars).collect()
    }
}

/// 读取（或首次生成）链盐（`audit_meta` 表 `chain_salt` 键；uuid v4 ×2 = 32 字节）。
fn ensure_chain_salt(conn: &Connection) -> DaemonResult<Vec<u8>> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT value FROM audit_meta WHERE key = 'chain_salt'",
            [],
            |row| row.get(0),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
        .map_err(|e| DaemonError::StorageError(format!("audit salt read: {e}")))?;
    if let Some(salt_hex) = existing {
        return hex::decode(&salt_hex)
            .map_err(|e| DaemonError::StorageError(format!("audit salt decode: {e}")));
    }
    // 首次生成：两个 uuid v4 = 32 字节随机盐（uuid crate 仅启用 v4 特性）。
    let mut salt = Vec::with_capacity(32);
    salt.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    salt.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    let salt_hex = hex::encode(&salt);
    conn.execute(
        "INSERT INTO audit_meta(key, value) VALUES('chain_salt', ?1)",
        params![salt_hex],
    )
    .map_err(|e| DaemonError::StorageError(format!("audit salt write: {e}")))?;
    Ok(salt)
}

/// 查询行 → 记录（u64 语义列以 i64 存储，读回钳制非负）。
#[allow(clippy::needless_pass_by_value)]
fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<AuditRecord> {
    let seq: i64 = row.get(0)?;
    let ts: i64 = row.get(1)?;
    Ok(AuditRecord {
        seq: u64::try_from(seq.max(0)).unwrap_or(u64::MAX),
        ts_ns: u64::try_from(ts.max(0)).unwrap_or(u64::MAX),
        actor: row.get(2)?,
        event: row.get(3)?,
        outcome: row.get(4)?,
        detail: row.get(5)?,
        prev_hash: row.get(6)?,
        entry_hash: row.get(7)?,
    })
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    /// 测试 IKM（部署路径由 env / 机器码注入；测试显式传字节）。
    const TEST_IKM: &[u8] = b"audit-test-ikm-2025";

    /// 受控纳秒时钟（测试注入）。
    #[derive(Clone)]
    struct TestClock(Arc<AtomicU64>);

    impl TestClock {
        fn at(ns: u64) -> Self {
            Self(Arc::new(AtomicU64::new(ns)))
        }
        fn advance(&self, ns: u64) {
            self.0.fetch_add(ns, Ordering::Relaxed);
        }
        fn clock(&self) -> Box<dyn Fn() -> u64 + Send + Sync> {
            let inner = self.0.clone();
            Box::new(move || inner.load(Ordering::Relaxed))
        }
    }

    /// 打开绑定受控时钟的审计库（独立临时文件）。
    fn open_logger(dir: &Path, clock: &TestClock) -> AuditLogger {
        AuditLogger::open(&dir.join("audit.db"), Some(TEST_IKM))
            .expect("open")
            .with_clock(clock.clock())
    }

    /// QA Happy（计划场景：配置修改 → 断言审计日志记录）:
    /// 记录 → 查询回读字段一致、seq 单调、事件类型过滤命中。
    #[test]
    fn record_then_query_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let logger = open_logger(dir.path(), &TestClock::at(1_000));

        logger
            .record(
                "admin",
                AuditEventType::ConfigChange,
                OUTCOME_ACCEPTED,
                "device_create dev-01",
            )
            .expect("record 1");
        logger
            .record(
                "intruder",
                AuditEventType::LoginFailed,
                OUTCOME_DENIED,
                "mgmt login",
            )
            .expect("record 2");

        // 全量回读。
        let rows = logger.query(&AuditQuery::new()).expect("query");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].seq, 1);
        assert_eq!(rows[0].actor, "admin");
        assert_eq!(rows[0].event, "config_change");
        assert_eq!(rows[0].outcome, OUTCOME_ACCEPTED);
        assert_eq!(rows[0].ts_ns, 1_000);
        assert_eq!(rows[1].seq, 2);
        assert_eq!(rows[1].ts_ns, 1_000);
        // 创始块 prev_hash + 链式串联。
        assert_eq!(rows[0].prev_hash, GENESIS_HASH);
        assert_eq!(rows[1].prev_hash, rows[0].entry_hash);

        // 事件类型过滤。
        let filtered = logger
            .query(&AuditQuery::new().with_event("login_failed"))
            .expect("query filter");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].event, "login_failed");
        assert_eq!(filtered[0].actor, "intruder");

        assert_eq!(logger.latest_seq().expect("latest"), 2);
    }

    /// QA: 时间窗过滤（闭区间）+ 分页（limit / offset）。
    #[test]
    fn query_time_window_and_pagination() {
        let dir = tempfile::tempdir().expect("tempdir");
        let clock = TestClock::at(1_000);
        let logger = open_logger(dir.path(), &clock);

        for i in 0..5 {
            logger
                .record(
                    "a",
                    AuditEventType::ConfigChange,
                    OUTCOME_ACCEPTED,
                    &format!("row-{i}"),
                )
                .expect("record");
            clock.advance(100); // ts = 1000, 1100, 1200, 1300, 1400
        }

        // since=1200（闭区间）→ 3 条。
        let rows = logger
            .query(&AuditQuery::new().with_since_ns(1_200))
            .expect("since");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].detail, "row-2");

        // until=1200（闭区间）→ 3 条。
        let rows = logger
            .query(&AuditQuery::new().with_until_ns(1_200))
            .expect("until");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2].detail, "row-2");

        // 分页：limit=2 offset=2 → 第 3、4 条。
        let rows = logger
            .query(&AuditQuery::new().with_limit(2).with_offset(2))
            .expect("page");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].detail, "row-2");
        assert_eq!(rows[1].detail, "row-3");

        // limit 超上限被钳制。
        let rows = logger
            .query(&AuditQuery::new().with_limit(u32::MAX))
            .expect("clamped");
        assert_eq!(rows.len(), 5);
    }

    /// QA（计划验收：日志追加不可篡改——正向）: 多条记录后整链校验通过。
    #[test]
    fn chain_verification_passes_on_intact_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        let logger = open_logger(dir.path(), &TestClock::at(10));

        for i in 0..16 {
            let event = if i % 2 == 0 {
                AuditEventType::Login
            } else {
                AuditEventType::AuthzFailed
            };
            logger
                .record("actor", event, OUTCOME_ACCEPTED, &format!("e{i}"))
                .expect("record");
        }

        let report = logger.verify_chain().expect("verify");
        assert!(report.ok, "intact chain must verify: {report:?}");
        assert_eq!(report.total, 16);
        assert_eq!(report.verified, 16);
        assert_eq!(report.first_broken_seq, None);
    }

    /// QA（计划验收核心：改一行 → 校验失败）: 绕过触发器（测试内先 DROP）
    /// 篡改一行 detail → 整链校验在**该行**断裂，首断点定位准确。
    #[test]
    fn tampered_row_breaks_chain_at_that_seq() {
        let dir = tempfile::tempdir().expect("tempdir");
        let logger = open_logger(dir.path(), &TestClock::at(10));
        for i in 0..4 {
            logger
                .record(
                    "actor",
                    AuditEventType::ConfigChange,
                    OUTCOME_ACCEPTED,
                    &format!("e{i}"),
                )
                .expect("record");
        }
        assert!(logger.verify_chain().expect("verify").ok);

        // 模拟攻击者：先摘除追加写触发器，再改第 3 行的 detail。
        let tamper = Connection::open(dir.path().join("audit.db")).expect("tamper conn");
        tamper
            .execute("DROP TRIGGER audit_log_no_update", [])
            .expect("drop trigger");
        tamper
            .execute("UPDATE audit_log SET detail = 'forged' WHERE seq = 3", [])
            .expect("tamper update");

        let report = logger.verify_chain().expect("verify");
        assert!(!report.ok, "forged row must break the chain: {report:?}");
        assert_eq!(report.first_broken_seq, Some(3), "break located at seq 3");
        assert_eq!(report.verified, 2, "rows 1..2 still verify");
        assert_eq!(report.total, 4);
    }

    /// QA（计划场景：尝试删除日志 → 断言失败）: 追加写触发器拒绝 DELETE /
    /// UPDATE，日志物理上不可就地删改。
    #[test]
    fn append_only_triggers_reject_update_and_delete() {
        let dir = tempfile::tempdir().expect("tempdir");
        let logger = open_logger(dir.path(), &TestClock::at(10));
        logger
            .record("actor", AuditEventType::Login, OUTCOME_ACCEPTED, "e0")
            .expect("record");

        let raw = Connection::open(dir.path().join("audit.db")).expect("raw conn");
        // DELETE → RAISE(ABORT)。
        let err = raw
            .execute("DELETE FROM audit_log WHERE seq = 1", [])
            .expect_err("delete blocked");
        let message = err.to_string();
        assert!(
            message.contains("append-only"),
            "delete must be rejected by trigger: {message}"
        );
        // UPDATE → RAISE(ABORT)。
        let err = raw
            .execute("UPDATE audit_log SET detail = 'x' WHERE seq = 1", [])
            .expect_err("update blocked");
        assert!(
            err.to_string().contains("append-only"),
            "update must be rejected by trigger: {err}"
        );
        // 日志仍然完好：删除未生效。
        assert_eq!(logger.latest_seq().expect("latest"), 1);
        assert!(logger.verify_chain().expect("verify").ok);
    }

    /// QA: 删行（而非改行）同样可检测——seq 连续性在断裂点定位。
    #[test]
    fn deleted_middle_row_breaks_seq_continuity() {
        let dir = tempfile::tempdir().expect("tempdir");
        let logger = open_logger(dir.path(), &TestClock::at(10));
        for i in 0..4 {
            logger
                .record(
                    "actor",
                    AuditEventType::ConfigChange,
                    OUTCOME_ACCEPTED,
                    &format!("e{i}"),
                )
                .expect("record");
        }
        let tamper = Connection::open(dir.path().join("audit.db")).expect("tamper conn");
        tamper
            .execute("DROP TRIGGER audit_log_no_delete", [])
            .expect("drop trigger");
        tamper
            .execute("DELETE FROM audit_log WHERE seq = 2", [])
            .expect("delete middle row");

        let report = logger.verify_chain().expect("verify");
        assert!(!report.ok, "missing row must break the chain: {report:?}");
        assert_eq!(
            report.first_broken_seq,
            Some(2),
            "gap located at missing seq"
        );
    }

    /// QA: 错误 IKM 打开同一库 → 链校验失败（密钥不硬编码、不随库走，
    /// 换密钥即不可伪造既有链）。
    #[test]
    fn wrong_ikm_fails_verification() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let logger =
                AuditLogger::open(&dir.path().join("audit.db"), Some(b"right-ikm")).expect("open");
            logger
                .record("actor", AuditEventType::Login, OUTCOME_ACCEPTED, "e0")
                .expect("record");
        }
        let other =
            AuditLogger::open(&dir.path().join("audit.db"), Some(b"wrong-ikm")).expect("reopen");
        let report = other.verify_chain().expect("verify");
        assert!(
            !report.ok,
            "wrong-key reopen must not verify existing chain"
        );
        assert_eq!(report.first_broken_seq, Some(1));
    }

    /// QA: 重开同一库（同 IKM）→ 盐复用、链校验仍通过（持久化 + 密钥派生跨重启稳定）。
    #[test]
    fn reopen_preserves_chain_and_salt() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let logger = open_logger(dir.path(), &TestClock::at(10));
            logger
                .record("actor", AuditEventType::Login, OUTCOME_ACCEPTED, "e0")
                .expect("record");
        }
        let clock2 = TestClock::at(999);
        let logger = AuditLogger::open(&dir.path().join("audit.db"), Some(TEST_IKM))
            .expect("reopen")
            .with_clock(clock2.clock());
        logger
            .record("actor", AuditEventType::LoginFailed, OUTCOME_DENIED, "e1")
            .expect("record 2");

        assert_eq!(logger.latest_seq().expect("latest"), 2);
        let rows = logger.query(&AuditQuery::new()).expect("query");
        assert_eq!(rows[0].ts_ns, 10, "first row timestamp preserved");
        assert_eq!(rows[1].ts_ns, 999);
        assert_eq!(rows[1].prev_hash, rows[0].entry_hash);
        assert!(logger.verify_chain().expect("verify").ok);
    }

    /// QA 红线: JSON 编码——seq / ts_ns 一律字符串（大数不丢精度）。
    /// ts 取真实量级的纳秒时间戳（超 JS 2^53−1 安全整数、但为合法 UNIX 纳秒）。
    #[test]
    fn json_encodes_u64_as_strings() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ts = 1_700_000_000_123_456_789u64;
        let clock = TestClock::at(ts);
        let logger = open_logger(dir.path(), &clock);
        logger
            .record(
                "a",
                AuditEventType::TrialExpired,
                OUTCOME_ACCEPTED,
                "trial end",
            )
            .expect("record");

        let rows = logger.query(&AuditQuery::new()).expect("query");
        let value = rows[0].to_json();
        assert!(value["seq"].is_string(), "seq must be string: {value}");
        assert!(value["ts_ns"].is_string(), "ts_ns must be string: {value}");
        assert_eq!(value["seq"], "1");
        assert_eq!(
            value["ts_ns"],
            ts.to_string(),
            "ns timestamp must round-trip losslessly"
        );
        assert_eq!(value["event"], "trial_expired");
    }

    /// QA: actor / detail 防御性截断 + 空 actor 落 `<missing>`。
    #[test]
    fn long_fields_are_clamped_and_empty_actor_marked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let logger = open_logger(dir.path(), &TestClock::at(10));
        let long_detail = "x".repeat(MAX_DETAIL_CHARS + 500);
        logger
            .record("", AuditEventType::Login, OUTCOME_ACCEPTED, &long_detail)
            .expect("record");

        let rows = logger.query(&AuditQuery::new()).expect("query");
        assert_eq!(rows[0].actor, "<missing>");
        assert_eq!(rows[0].detail.chars().count(), MAX_DETAIL_CHARS);
    }

    /// QA: AuditEventType 字面量与解析互逆（HTTP 过滤参数校验的契约）。
    #[test]
    fn event_type_roundtrip() {
        for event in [
            AuditEventType::Login,
            AuditEventType::LoginFailed,
            AuditEventType::ConfigChange,
            AuditEventType::AuthzFailed,
            AuditEventType::TrialExpired,
            AuditEventType::AuditRead,
            AuditEventType::AuditExport,
        ] {
            assert_eq!(AuditEventType::parse(event.as_str()), Some(event));
        }
        assert_eq!(AuditEventType::parse("nope"), None);
        assert_eq!(AuditEventType::parse(""), None);
    }

    /// QA: IKM 解析——机器码回退生效、空机器码与空 env 视为缺失。
    /// （env 优先级路径依赖进程级环境变量，不在并行测试中改写 env，此处不覆盖。）
    #[test]
    fn resolve_audit_ikm_fallbacks() {
        let from_machine = resolve_audit_ikm(Some("  machine-code-1  "));
        assert_eq!(
            from_machine.as_deref(),
            Some(b"machine-code-1".as_slice()),
            "machine code fallback (trimmed)"
        );
        assert_eq!(resolve_audit_ikm(Some("   ")), None, "blank machine code");
        assert_eq!(resolve_audit_ikm(None), None, "nothing available");
    }
}
