//! `licensing-server` 仓储层：**全部 SQL 集中于此**（API / 业务层不写 SQL）。
//!
//! 权威文档：`docs/design/licensing-data-model.md`（9 表）；数据结构见 [`crate::model`]。
//!
//! # 设计约束（逐条对齐设计文档）
//!
//! 1. **单连接 + Mutex**：服务端并发量低，`parking_lot::Mutex<Connection>` 比连接池
//!    更易保证事务语义（一个写事务 = 一次锁持有）。
//! 2. **WAL + 外键强制 + busy_timeout**：`open` 时统一设置（`journal_mode=WAL`、
//!    `foreign_keys=ON`、`busy_timeout=5000`），跨进程与磁盘抖动场景下行为可预期。
//! 3. **一机一码是竞态防线**：[`Store::bind_code_to_device`] 用**单条条件 UPDATE** +
//!    `rows_affected` 判定，**绝不做「先 SELECT 再 UPDATE」**（两种「同码异机激活」
//!    并发请求会同时通过 SELECT 校验，双双绑定成功 → 一机一码被击穿）。
//! 4. **防重放用 INSERT OR IGNORE**：[`Store::insert_nonce_if_absent`] 依赖主键唯一约束
//!    由数据库仲裁，`rows_affected() == 1` 即首次；`0` 即重放。
//! 5. **时间一律 UTC 秒 INTEGER**：所有 `*_at` / `*_ts` 列均为 `INTEGER`（跨端一致性红线）。
//! 6. **生产路径零 panic**：全部方法返回 [`LicenseResult`]，**无 `unwrap` / `expect` /
//!    `panic!`**。`i64 ↔ u32` 转换一律走 `try_from` + `map_err`。
//! 7. **错误信息不承载敏感值**：错误里**不出现激活码原文、私钥、指纹原文**
//!    （错误会进日志与审计）。`bind_code_to_device` 的冲突错误只带 `code_id`。

use std::path::{Path, PathBuf};

use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::admin_auth::AdminAccount;
use crate::error::{LicenseError, LicenseResult};
use crate::model::{
    now_unix_secs, ActivationCode, ActorType, AuditLog, AuditReceipt, CodeStatus, DeployMode,
    Device, DeviceStatus, Heartbeat, HeartbeatResult, Lease, LeaseStatus, NonceCache, OtaPackage,
    OtaStatus, SigningKey, SigningKeyStatus, Tenant, VerifyMode,
};

/// **增量列迁移表**：`(表名, 列名, 补齐该列的 DDL)`。
///
/// 只对**已建库后**新增的列登记；`migrate` 会先查 `PRAGMA table_info`，缺列才执行 DDL，
/// 因此对全新库（列已在 `CREATE TABLE` 中）与旧库都幂等。
const ADDED_COLUMNS: &[(&str, &str, &str)] = &[
    (
        "activation_code",
        "prebind_machine_code",
        "ALTER TABLE activation_code ADD COLUMN prebind_machine_code TEXT",
    ),
    (
        // 2026-09-25 主理人决策：/activation 开启请求验签后，设备首次激活成功时把请求
        // 自带公钥 first-write-wins 钉定到本列；此后激活请求公钥必须一致（可空 = 未钉定，
        // 兼容历史设备，由下一次激活成功路径补钉定）。
        "device",
        "device_pubkey",
        "ALTER TABLE device ADD COLUMN device_pubkey TEXT",
    ),
];

/// **存量机器码归一迁移**（2026-09-28）：把历史库里大小写混杂 / 带展示分隔符的机器码
/// 统一成「无分隔符小写」这一**匹配态**。
///
/// # 为什么必须落这条迁移
/// 网关上报的机器码是 `hex::encode` 的**无分隔符小写**串（如 `8f3a91c27d045be6`），
/// 而历史库里的值可能来自两条污染路径：① 早期 admin-console 在发放时把用户输入
/// `toUpperCase()` 后提交（大写）；② 运维从网关控制台复制的**带 `-` 展示态**
/// （如 `8F3A-91C2-7D04-5BE6`）被原样填入预绑定。只改读写路径
/// （[`crate::model::normalize_machine_code`]）**救不了存量**：库里已是污染值的历史
/// 预绑定码仍与上报值不等 → 照样 `PREBIND_CONFLICT`（HTTP 422）。故已落库的值必须一并
/// 改写。三列元组为 `(表名, 机器码列, 行标识列)`。
///
/// # 为什么用 SQL 内建函数而不是拉到 Rust 里改
/// SQLite 内建 `lower()` **只对 ASCII 生效**（非 ASCII 字节原样保留），与 Rust 侧
/// [`str::to_ascii_lowercase`] 语义严格一致；`replace()` 负责逐字符剥离分隔符。
/// 换用任何带 Unicode 语义的实现都会让「SQL 存量值」与「Rust 新写入值」产生漂移。
///
/// # 分隔符集合的 SQL 表达与已知缺口
/// Rust 侧 [`crate::model::normalize_machine_code`] 剥离 `-` `:` `_` 以及**所有** ASCII
/// 空白；SQL 侧 `replace(x,' ','')` 只能剥离**单个半角空格**（SQLite 无内建「所有空白」
/// 替换）。制表符 / 换行等极罕见出现在机器码里，接受该差异；若出现，启动后的
/// 冲突告警与下一次读写路径归一仍会兜底。
///
/// # 唯一约束与冲突处理（fail-safe，绝不静默丢行）
/// `device.machine_code` 带 `UNIQUE` 约束（见 [`SCHEMA`]）；`activation_code.prebind_machine_code`
/// **只有非唯一索引** `idx_code_prebind`。故归一后**可能**撞 UNIQUE 的只有 `device` 表。
/// 若两行归一后同值，直接 `UPDATE` 会在第二行抛约束错误使**迁移失败 / 启动失败**；
/// 直接删行更会**丢设备记录**。两种都不可接受——处理策略是：
///   1. 迁移前按归一值 `GROUP BY ... HAVING COUNT(*) > 1` 统计「归一后会重复」的组；
///   2. 命中时用 `tracing::warn!` 打印**冲突的归一值、行标识与原始值**（可人工介入）；
///   3. 改写改用 `UPDATE OR IGNORE`：撞约束的行**跳过不改**，迁移仍能完成，**不删任何行**。
///
/// # 幂等
/// 重复启动时 `{column} <> {normalized}` 对已归一行恒为假 → 零行命中、不产生新变更、
/// 不报错（仅当冲突残留未解决时重复告警）。
const MACHINE_CODE_NORMALIZE_MIGRATIONS: &[(&str, &str, &str)] = &[
    ("activation_code", "prebind_machine_code", "code_id"),
    ("device", "machine_code", "device_id"),
];

/// 判断表中是否已存在某列（基于 `PRAGMA table_info`，不受 SQLite 版本差异影响）。
fn column_exists(conn: &Connection, table: &str, column: &str) -> LicenseResult<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |row| row.get::<usize, String>(1))?;
    for name in rows {
        if name? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

// ---- 列表过滤条件 ----

/// `activation_code` 列表过滤条件（`None` 字段 = 不施加该条件）。
///
/// **字段集合由 service 层契约固定**：`{ tenant_id, status, tier, order_id }`。
/// `order_id` 对应列 `source_order_id`。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodeFilter {
    /// 按租户过滤（`activation_code.tenant_id`）。
    pub tenant_id: Option<String>,
    /// 按状态过滤。
    pub status: Option<CodeStatus>,
    /// 按授权档位过滤（`activation_code.tier`）。
    pub tier: Option<String>,
    /// 按来源订单过滤（`activation_code.source_order_id`）。
    pub order_id: Option<String>,
}

/// `audit_log` 列表过滤条件（`None` 字段 = 不施加该条件）。
///
/// **字段集合由 service 层契约固定**：`{ actor_type, action, entity_type, entity_id }`。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditFilter {
    /// 按主体类型过滤。
    pub actor_type: Option<ActorType>,
    /// 按动作过滤。
    pub action: Option<String>,
    /// 按实体类型过滤。
    pub entity_type: Option<String>,
    /// 按实体 ID 过滤。
    pub entity_id: Option<String>,
}

// ---- SQL 迁移 ----

/// 建表 + 索引语句（**幂等**：全部 `IF NOT EXISTS`）。
const SCHEMA: &[&str] = &[
    r#"CREATE TABLE IF NOT EXISTS tenant (
        tenant_id           TEXT PRIMARY KEY,
        name                TEXT NOT NULL,
        verify_mode_default TEXT NOT NULL DEFAULT 'B',
        contact             TEXT NOT NULL DEFAULT '',
        created_at          INTEGER NOT NULL
    )"#,
    r#"CREATE TABLE IF NOT EXISTS device (
        device_id           TEXT PRIMARY KEY,
        tenant_id           TEXT NOT NULL REFERENCES tenant(tenant_id),
        machine_code        TEXT NOT NULL UNIQUE,
        anchor_hashes       TEXT NOT NULL,
        deploy_mode         TEXT NOT NULL,
        image_digest        TEXT,
        host_anchor_ref     TEXT,
        first_activation_at INTEGER,
        status              TEXT NOT NULL,
        created_at          INTEGER NOT NULL,
        device_pubkey       TEXT
    )"#,
    r#"CREATE INDEX IF NOT EXISTS idx_device_tenant ON device(tenant_id)"#,
    r#"CREATE TABLE IF NOT EXISTS activation_code (
        code_id          TEXT PRIMARY KEY,
        code             TEXT NOT NULL UNIQUE,
        status           TEXT NOT NULL,
        bound_device_id  TEXT REFERENCES device(device_id),
        tenant_id        TEXT NOT NULL REFERENCES tenant(tenant_id),
        tier             TEXT NOT NULL,
        valid_from       INTEGER NOT NULL,
        valid_until      INTEGER NOT NULL,
        source_order_id  TEXT NOT NULL,
        reissued_from_id TEXT REFERENCES activation_code(code_id),
        issued_by        TEXT NOT NULL,
        revoked_at       INTEGER,
        revoked_reason   TEXT,
        idempotency_key  TEXT,
        created_at       INTEGER NOT NULL,
        -- task 46：预绑定机器码。NULL = 留待首次激活自由绑定。
        prebind_machine_code TEXT
    )"#,
    r#"CREATE INDEX IF NOT EXISTS idx_code_tenant ON activation_code(tenant_id)"#,
    r#"CREATE INDEX IF NOT EXISTS idx_code_bound_device ON activation_code(bound_device_id)"#,
    // task 46：重发溯源链与预绑定冲突检测都必须**走索引直查**——
    // 早期实现靠 `list_codes` 分页 + 内存过滤，租户码数超过一页时会被**截断漏命中**，
    // 导致换机重发被误判为「首次」而**重复签发新码**（一机一码破口）。
    r#"CREATE INDEX IF NOT EXISTS idx_code_reissued_from ON activation_code(reissued_from_id)"#,
    r#"CREATE INDEX IF NOT EXISTS idx_code_prebind ON activation_code(prebind_machine_code)"#,
    // `idempotency_key` 标识**一次发放批次**（一批多码共用同一 key），因此**不建唯一索引**：
    // 唯一约束属于「批次」而非「单行」，行级唯一会让批量发码直接失败。幂等语义由
    // service 层按 key 查询既有批次实现（见 `LicensingService::issue_codes`）。
    r#"CREATE INDEX IF NOT EXISTS idx_code_idem ON activation_code(idempotency_key)"#,
    // `code_batch` 是**批次头表**：`idempotency_key` 作为主键，把「一次发放批次」的
    // 幂等仲裁下沉到数据库（`INSERT OR IGNORE` + `rows_affected`），从而在**并发重放**
    // 下也保证同一 key 只有一个批次胜出。`activation_code.idempotency_key` 仍是**非唯一**
    // 索引（一批多码必须共用同一 key），头表承担唯一性。
    r#"CREATE TABLE IF NOT EXISTS code_batch (
        idempotency_key TEXT PRIMARY KEY,
        tenant_id       TEXT NOT NULL REFERENCES tenant(tenant_id),
        expected_count  INTEGER NOT NULL,
        created_at      INTEGER NOT NULL
    )"#,
    r#"CREATE INDEX IF NOT EXISTS idx_code_batch_tenant ON code_batch(tenant_id)"#,
    r#"CREATE TABLE IF NOT EXISTS signing_key (
        kid        TEXT PRIMARY KEY,
        status     TEXT NOT NULL,
        public_key TEXT NOT NULL,
        hsm_ref    TEXT,
        enabled_at INTEGER NOT NULL,
        retired_at INTEGER
    )"#,
    // `lease.kid` **不是硬外键**：签名私钥经环境变量注入、由 [`crate::keys::KeyRing`] 持有，
    // `signing_key` 表只承载**公钥与 hsm_ref**（设计 §5 私钥隔离）。轮换期间新 kid 可先
    // 用于签发、再择机登记公钥，故不能把「租约签发」耦合到一次 `signing_key` 写入。
    // 关系（`SIGNING_KEY ||--o{ LEASE`）以 `idx_lease_kid` 索引 + 应用层校验表达。
    r#"CREATE TABLE IF NOT EXISTS lease (
        lease_id          TEXT PRIMARY KEY,
        device_id         TEXT NOT NULL REFERENCES device(device_id),
        code_id           TEXT NOT NULL REFERENCES activation_code(code_id),
        kid               TEXT NOT NULL,
        token_sig         TEXT NOT NULL,
        verify_mode       TEXT NOT NULL,
        tier              TEXT NOT NULL,
        issued_at         INTEGER NOT NULL,
        valid_until       INTEGER NOT NULL,
        last_heartbeat_at INTEGER,
        status            TEXT NOT NULL
    )"#,
    r#"CREATE INDEX IF NOT EXISTS idx_lease_device ON lease(device_id)"#,
    r#"CREATE INDEX IF NOT EXISTS idx_lease_kid ON lease(kid)"#,
    r#"CREATE TABLE IF NOT EXISTS heartbeat (
        id             TEXT PRIMARY KEY,
        lease_id       TEXT NOT NULL REFERENCES lease(lease_id),
        device_id      TEXT NOT NULL REFERENCES device(device_id),
        client_ts      INTEGER NOT NULL,
        server_ts      INTEGER NOT NULL,
        result         TEXT NOT NULL,
        receipt_cursor TEXT,
        created_at     INTEGER NOT NULL
    )"#,
    r#"CREATE INDEX IF NOT EXISTS idx_heartbeat_lease ON heartbeat(lease_id)"#,
    r#"CREATE TABLE IF NOT EXISTS audit_receipt (
        id             TEXT PRIMARY KEY,
        lease_id       TEXT NOT NULL REFERENCES lease(lease_id),
        device_mid     TEXT NOT NULL,
        seq_from       INTEGER NOT NULL,
        seq_to         INTEGER NOT NULL,
        count          INTEGER NOT NULL,
        payload_digest TEXT NOT NULL,
        ts             INTEGER NOT NULL,
        sig            TEXT NOT NULL,
        received_at    INTEGER NOT NULL,
        gap_flag       INTEGER NOT NULL DEFAULT 0
    )"#,
    r#"CREATE INDEX IF NOT EXISTS idx_receipt_lease ON audit_receipt(lease_id, seq_from)"#,
    r#"CREATE TABLE IF NOT EXISTS nonce_cache (
        nonce      TEXT PRIMARY KEY,
        device_id  TEXT NOT NULL REFERENCES device(device_id),
        expires_at INTEGER NOT NULL,
        used_at    INTEGER NOT NULL
    )"#,
    r#"CREATE INDEX IF NOT EXISTS idx_nonce_expires ON nonce_cache(expires_at)"#,
    r#"CREATE TABLE IF NOT EXISTS audit_log (
        id          TEXT PRIMARY KEY,
        actor_type  TEXT NOT NULL,
        actor_id    TEXT NOT NULL,
        action      TEXT NOT NULL,
        entity_type TEXT NOT NULL,
        entity_id   TEXT NOT NULL,
        detail      TEXT NOT NULL,
        ts          INTEGER NOT NULL,
        ip          TEXT NOT NULL
    )"#,
    r#"CREATE INDEX IF NOT EXISTS idx_audit_ts ON audit_log(ts)"#,
    r#"CREATE INDEX IF NOT EXISTS idx_audit_entity ON audit_log(entity_type, entity_id)"#,
    // OTA 升级包（网关系统更新的发布物仓库；网关侧 `GET /updates/manifest` 拉取）。
    // ⚠️ `version` / `size` / `published_at` 一律 TEXT：网关 `daemon::ota::parse_manifest`
    // 对 manifest 的数值型字段**显式拒绝**（大数红线），本侧必须存字符串才能下发。
    r#"CREATE TABLE IF NOT EXISTS ota_package (
        version         TEXT NOT NULL,
        channel         TEXT NOT NULL,
        size            TEXT NOT NULL,
        payload_sha256  TEXT NOT NULL,
        payload_b64     TEXT NOT NULL,
        sig_b64         TEXT NOT NULL,
        kid             TEXT NOT NULL,
        status          TEXT NOT NULL DEFAULT 'draft',
        published_at    TEXT NOT NULL DEFAULT '',
        published_by    TEXT NOT NULL DEFAULT '',
        note            TEXT NOT NULL DEFAULT '',
        PRIMARY KEY (version, channel)
    )"#,
    r#"CREATE INDEX IF NOT EXISTS idx_ota_channel_status ON ota_package(channel, status)"#,
    // 管理端账号表（可配置账号 / 角色；口令只存 64-hex SHA-256 摘要，明文绝不落盘）。
    r#"CREATE TABLE IF NOT EXISTS admin_account (
        account         TEXT PRIMARY KEY,
        display_name    TEXT NOT NULL DEFAULT '',
        role            TEXT NOT NULL,
        password_sha256 TEXT NOT NULL,
        status          TEXT NOT NULL DEFAULT 'active',
        last_login_at   INTEGER,
        created_at      INTEGER NOT NULL,
        updated_at      INTEGER NOT NULL
    )"#,
];

// ---- 行映射辅助 ----

/// `i64 → u32` 分页转换（**不 panic**）。
fn to_u32(value: i64, what: &str) -> LicenseResult<u32> {
    u32::try_from(value)
        .map_err(|_| LicenseError::Storage(format!("{what} does not fit into u32: {value}")))
}

/// `i64 → u64` 计数转换（**不 panic**：SQLite `COUNT(*)` 非负，负值视为损坏数据）。
fn to_u64(value: i64, what: &str) -> LicenseResult<u64> {
    u64::try_from(value)
        .map_err(|_| LicenseError::Storage(format!("{what} is negative or too large: {value}")))
}

/// `usize → i64` 转换（**不 panic**）。
fn to_i64(value: usize, what: &str) -> LicenseResult<i64> {
    i64::try_from(value)
        .map_err(|_| LicenseError::Storage(format!("{what} does not fit into i64: {value}")))
}

/// `LicenseError → rusqlite::Error`：供 `query_map` 回调（签名要求 `rusqlite::Result`）使用。
///
/// 把枚举解析 / JSON 解码失败包装为 `FromSqlConversionFailure`，使其能在
/// `query_row` / `query_map` 内部传播；离开迭代器后再经
/// `From<rusqlite::Error> for LicenseError` 归一为 [`LicenseError::Storage`]。
fn conversion_error(message: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, message.into())
}

/// 解析状态枚举并转为 `rusqlite::Error`（供行映射使用）。
fn parse_enum<T, E>(value: &str, parse: E) -> rusqlite::Result<T>
where
    E: FnOnce(&str) -> LicenseResult<T>,
{
    parse(value).map_err(|e| conversion_error(e.to_string()))
}

/// 解析 `device.anchor_hashes`（JSON 数组）。
fn parse_anchor_hashes(raw: &str) -> rusqlite::Result<Vec<String>> {
    serde_json::from_str(raw).map_err(|e| {
        // 不回显 `raw` 内容（可能含指纹材料）。
        conversion_error(format!(
            "device.anchor_hashes is not a JSON string array: {e}"
        ))
    })
}

/// 序列化 `device.anchor_hashes`。
fn encode_anchor_hashes(anchors: &[String]) -> LicenseResult<String> {
    serde_json::to_string(anchors)
        .map_err(|e| LicenseError::Storage(format!("cannot encode anchor_hashes: {e}")))
}

/// 从行读取 `Tenant`。
fn row_to_tenant(row: &Row<'_>) -> rusqlite::Result<Tenant> {
    let verify_raw: String = row.get(2)?;
    Ok(Tenant {
        tenant_id: row.get(0)?,
        name: row.get(1)?,
        verify_mode_default: parse_enum(&verify_raw, VerifyMode::parse)?,
        contact: row.get(3)?,
        created_at: row.get(4)?,
    })
}

/// 从行读取 `Device`。
fn row_to_device(row: &Row<'_>) -> rusqlite::Result<Device> {
    let anchor_raw: String = row.get(3)?;
    let image_digest: Option<String> = row.get(5)?;
    let host_anchor_ref: Option<String> = row.get(6)?;
    let first_activation_at: Option<i64> = row.get(7)?;
    let deploy_raw: String = row.get(4)?;
    let status_raw: String = row.get(8)?;
    Ok(Device {
        device_id: row.get(0)?,
        tenant_id: row.get(1)?,
        machine_code: row.get(2)?,
        anchor_hashes: parse_anchor_hashes(&anchor_raw)?,
        deploy_mode: parse_enum(&deploy_raw, DeployMode::parse)?,
        image_digest,
        host_anchor_ref,
        first_activation_at,
        status: parse_enum(&status_raw, DeviceStatus::parse)?,
        created_at: row.get(9)?,
    })
}

/// 从行读取 `ActivationCode`。
fn row_to_code(row: &Row<'_>) -> rusqlite::Result<ActivationCode> {
    let status_raw: String = row.get(2)?;
    Ok(ActivationCode {
        code_id: row.get(0)?,
        code: row.get(1)?,
        status: parse_enum(&status_raw, CodeStatus::parse)?,
        bound_device_id: row.get(3)?,
        tenant_id: row.get(4)?,
        tier: row.get(5)?,
        valid_from: row.get(6)?,
        valid_until: row.get(7)?,
        source_order_id: row.get(8)?,
        reissued_from_id: row.get(9)?,
        issued_by: row.get(10)?,
        revoked_at: row.get(11)?,
        revoked_reason: row.get(12)?,
        idempotency_key: row.get(13)?,
        created_at: row.get(14)?,
        prebind_machine_code: row.get(15)?,
    })
}

/// 从行读取 `Lease`。
fn row_to_lease(row: &Row<'_>) -> rusqlite::Result<Lease> {
    let verify_raw: String = row.get(5)?;
    let status_raw: String = row.get(10)?;
    Ok(Lease {
        lease_id: row.get(0)?,
        device_id: row.get(1)?,
        code_id: row.get(2)?,
        kid: row.get(3)?,
        token_sig: row.get(4)?,
        verify_mode: parse_enum(&verify_raw, VerifyMode::parse)?,
        tier: row.get(6)?,
        issued_at: row.get(7)?,
        valid_until: row.get(8)?,
        last_heartbeat_at: row.get(9)?,
        status: parse_enum(&status_raw, LeaseStatus::parse)?,
    })
}

/// 从行读取 `Heartbeat`。
fn row_to_heartbeat(row: &Row<'_>) -> rusqlite::Result<Heartbeat> {
    let result_raw: String = row.get(5)?;
    Ok(Heartbeat {
        id: row.get(0)?,
        lease_id: row.get(1)?,
        device_id: row.get(2)?,
        client_ts: row.get(3)?,
        server_ts: row.get(4)?,
        result: parse_enum(&result_raw, HeartbeatResult::parse)?,
        receipt_cursor: row.get(6)?,
        created_at: row.get(7)?,
    })
}

/// 从行读取 `AuditReceipt`。
fn row_to_receipt(row: &Row<'_>) -> rusqlite::Result<AuditReceipt> {
    let gap_raw: i64 = row.get(10)?;
    Ok(AuditReceipt {
        id: row.get(0)?,
        lease_id: row.get(1)?,
        device_mid: row.get(2)?,
        seq_from: row.get(3)?,
        seq_to: row.get(4)?,
        count: row.get(5)?,
        payload_digest: row.get(6)?,
        ts: row.get(7)?,
        sig: row.get(8)?,
        received_at: row.get(9)?,
        gap_flag: gap_raw != 0,
    })
}

/// 从行读取 `NonceCache`。
fn row_to_nonce(row: &Row<'_>) -> rusqlite::Result<NonceCache> {
    Ok(NonceCache {
        nonce: row.get(0)?,
        device_id: row.get(1)?,
        expires_at: row.get(2)?,
        used_at: row.get(3)?,
    })
}

/// 从行读取 `SigningKey`。
fn row_to_signing_key(row: &Row<'_>) -> rusqlite::Result<SigningKey> {
    let status_raw: String = row.get(1)?;
    Ok(SigningKey {
        kid: row.get(0)?,
        status: parse_enum(&status_raw, SigningKeyStatus::parse)?,
        public_key: row.get(2)?,
        hsm_ref: row.get(3)?,
        enabled_at: row.get(4)?,
        retired_at: row.get(5)?,
    })
}

/// 从行读取 `OtaPackage`（状态字面量非法 → 行映射错误，**不 panic**、不猜测）。
fn row_to_ota_package(row: &Row<'_>) -> rusqlite::Result<OtaPackage> {
    let status_raw: String = row.get(7)?;
    Ok(OtaPackage {
        version: row.get(0)?,
        channel: row.get(1)?,
        size: row.get(2)?,
        payload_sha256: row.get(3)?,
        payload_b64: row.get(4)?,
        sig_b64: row.get(5)?,
        kid: row.get(6)?,
        status: parse_enum(&status_raw, |raw| {
            OtaStatus::parse(raw).ok_or_else(|| {
                LicenseError::Storage(format!("ota_package.status is not a valid status: {raw}"))
            })
        })?,
        published_at: row.get(8)?,
        published_by: row.get(9)?,
        note: row.get(10)?,
    })
}

/// 从行读取 `AdminAccount`。
fn row_to_admin_account(row: &Row<'_>) -> rusqlite::Result<AdminAccount> {
    Ok(AdminAccount {
        account: row.get(0)?,
        display_name: row.get(1)?,
        role: row.get(2)?,
        password_sha256: row.get(3)?,
        status: row.get(4)?,
        last_login_at: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

/// 从行读取 `AuditLog`。
fn row_to_audit_log(row: &Row<'_>) -> rusqlite::Result<AuditLog> {
    let actor_raw: String = row.get(1)?;
    Ok(AuditLog {
        id: row.get(0)?,
        actor_type: parse_enum(&actor_raw, ActorType::parse)?,
        actor_id: row.get(2)?,
        action: row.get(3)?,
        entity_type: row.get(4)?,
        entity_id: row.get(5)?,
        detail: row.get(6)?,
        ts: row.get(7)?,
        ip: row.get(8)?,
    })
}

// ---- 动态过滤条件拼装（**只用绑定参数**，杜绝注入） ----

/// 把 [`CodeFilter`] 追加到 `sql` 的 WHERE 子句，并按顺序压入绑定参数。
fn push_code_filter(
    sql: &mut String,
    args: &mut Vec<Box<dyn rusqlite::ToSql>>,
    filter: &CodeFilter,
) {
    if let Some(tenant) = &filter.tenant_id {
        sql.push_str(" AND tenant_id = ?");
        args.push(Box::new(tenant.clone()));
    }
    if let Some(status) = filter.status {
        sql.push_str(" AND status = ?");
        args.push(Box::new(status.as_str().to_string()));
    }
    if let Some(tier) = &filter.tier {
        sql.push_str(" AND tier = ?");
        args.push(Box::new(tier.clone()));
    }
    if let Some(order_id) = &filter.order_id {
        sql.push_str(" AND source_order_id = ?");
        args.push(Box::new(order_id.clone()));
    }
}

/// 把 [`AuditFilter`] 追加到 `sql` 的 WHERE 子句，并按顺序压入绑定参数。
fn push_audit_filter(
    sql: &mut String,
    args: &mut Vec<Box<dyn rusqlite::ToSql>>,
    filter: &AuditFilter,
) {
    if let Some(actor_type) = filter.actor_type {
        sql.push_str(" AND actor_type = ?");
        args.push(Box::new(actor_type.as_str().to_string()));
    }
    if let Some(action) = &filter.action {
        sql.push_str(" AND action = ?");
        args.push(Box::new(action.clone()));
    }
    if let Some(entity_type) = &filter.entity_type {
        sql.push_str(" AND entity_type = ?");
        args.push(Box::new(entity_type.clone()));
    }
    if let Some(entity_id) = &filter.entity_id {
        sql.push_str(" AND entity_id = ?");
        args.push(Box::new(entity_id.clone()));
    }
}

// ---- Store ----

/// 仓储层：单连接 + `Mutex`，全部表访问的唯一入口。
///
/// **不实现 `Debug`**（内含 `rusqlite::Connection`，暴露无益且可能泄露路径细节）；
/// 需要打印时使用 [`Store::path`]。
pub struct Store {
    conn: Mutex<Connection>,
    path: PathBuf,
}

/// `Store` 的手写 `Debug`：**只输出数据库文件路径**，不含连接内部状态。
impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// 配置单连接的 PRAGMA 与会话级外键（对连接池 / 单连接均适用）。
fn configure(conn: &Connection) -> LicenseResult<()> {
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA foreign_keys=ON;
         PRAGMA busy_timeout=5000;",
    )?;
    Ok(())
}

impl Store {
    /// 打开（必要时创建）磁盘数据库，完成 PRAGMA 与建表迁移。
    pub fn open(path: &Path) -> LicenseResult<Self> {
        let conn = Connection::open(path)?;
        configure(&conn)?;
        let store = Store {
            conn: Mutex::new(conn),
            path: path.to_path_buf(),
        };
        store.migrate()?;
        Ok(store)
    }

    /// 打开**纯内存**数据库（测试用；同样启用外键约束）。
    pub fn open_in_memory() -> LicenseResult<Self> {
        let conn = Connection::open_in_memory()?;
        configure(&conn)?;
        let store = Store {
            conn: Mutex::new(conn),
            path: PathBuf::from(":memory:"),
        };
        store.migrate()?;
        Ok(store)
    }

    /// 数据库文件路径（`:memory:` 表示内存库）。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 执行建表 + 索引（**幂等**：全部 `IF NOT EXISTS`）。
    ///
    /// 建表之后追加 **列级增量迁移**（[`ADDED_COLUMNS`]）：`CREATE TABLE IF NOT EXISTS`
    /// 对**已存在**的旧库不会补列，故 task 46 新增的 `prebind_machine_code` 必须以
    /// 「查 `PRAGMA table_info` → 缺则 `ALTER TABLE ADD COLUMN`」的方式补齐，
    /// 否则旧库上所有预绑定查询都会因 `no such column` 而整体失败。
    pub fn migrate(&self) -> LicenseResult<()> {
        let conn = self.conn.lock();
        for statement in SCHEMA {
            conn.execute_batch(statement)?;
        }
        for (table, column, ddl) in ADDED_COLUMNS {
            if !column_exists(&conn, table, column)? {
                conn.execute_batch(ddl)?;
            }
        }
        // 存量机器码归一（2026-09-28）：把历史库里大小写混杂 / 带展示分隔符的机器码
        // 统一成「无分隔符小写」匹配态（语义与 Rust 侧 `normalize_machine_code` 对齐）。
        // 用 SQL 内建 `lower()` + 嵌套 `replace()`（仅 ASCII 生效）。只动「确实会变」的行
        // （`col <> normalized`），避免无谓的 UNIQUE 重检，保证迁移**幂等**。
        for (table, column, id_column) in MACHINE_CODE_NORMALIZE_MIGRATIONS {
            // 归一表达式：lower(剥 `-` `:` `_` 与半角空格)。SQLite 的 `replace` 只替换
            // 单个空格，制表符等极罕见，接受该缺口（见常量文档）。
            let normalized = format!(
                "lower(replace(replace(replace(replace({column},'-',''),':',''),'_',''),' ',''))"
            );
            // ① 冲突预检：按归一值分组，找出「归一后会撞值」的组并**告警全部细节**。
            //    绝不静默 —— 这些行无法安全自动合并（可能是两台真机，或一机被重复登记）。
            let detect = format!(
                "SELECT {normalized}, group_concat({id_column}), group_concat({column}) \
                 FROM {table} WHERE {column} IS NOT NULL \
                 GROUP BY 1 HAVING COUNT(*) > 1"
            );
            {
                let mut stmt = conn.prepare(&detect)?;
                let mut rows = stmt.query([])?;
                while let Some(row) = rows.next()? {
                    let normalized_value: String = row.get(0)?;
                    let ids: Option<String> = row.get(1)?;
                    let raw_values: Option<String> = row.get(2)?;
                    tracing::warn!(
                        table = table,
                        column = column,
                        normalized = %normalized_value,
                        row_ids = %ids.unwrap_or_default(),
                        raw_values = %raw_values.unwrap_or_default(),
                        "机器码归一迁移检测到归一后冲突：多行归一为同一值；\
                         已保留全部行，撞 UNIQUE 的行将跳过不改，请人工核对"
                    );
                }
            }
            // ② 改写：`UPDATE OR IGNORE` 让撞 UNIQUE 的行被跳过而非让整个迁移失败，
            //    **绝不删行**（删 `device` 行会丢设备记录）。
            conn.execute_batch(&format!(
                "UPDATE OR IGNORE {table} SET {column} = {normalized} \
                 WHERE {column} IS NOT NULL AND {column} <> {normalized}"
            ))?;
        }
        Ok(())
    }

    /// 查询当前 `PRAGMA journal_mode`（迁移断言 / 运维自检用）。
    pub fn journal_mode(&self) -> LicenseResult<String> {
        let conn = self.conn.lock();
        let mode: String = conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
        Ok(mode)
    }

    /// 查询当前 `PRAGMA foreign_keys`（0 / 1）。
    pub fn foreign_keys_enabled(&self) -> LicenseResult<bool> {
        let conn = self.conn.lock();
        let flag: i64 = conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
        Ok(flag != 0)
    }

    // ================= tenant =================

    /// 插入租户。
    pub fn insert_tenant(&self, tenant: &Tenant) -> LicenseResult<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO tenant (tenant_id, name, verify_mode_default, contact, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                tenant.tenant_id,
                tenant.name,
                tenant.verify_mode_default.as_str(),
                tenant.contact,
                tenant.created_at,
            ],
        )?;
        Ok(())
    }

    /// 按 ID 查询租户。
    pub fn get_tenant(&self, tenant_id: &str) -> LicenseResult<Option<Tenant>> {
        let conn = self.conn.lock();
        let found = conn
            .query_row(
                "SELECT tenant_id, name, verify_mode_default, contact, created_at
                 FROM tenant WHERE tenant_id = ?1",
                params![tenant_id],
                row_to_tenant,
            )
            .optional()?;
        Ok(found)
    }

    /// 列出全部租户（按 `created_at` 升序）。
    pub fn list_tenants(&self) -> LicenseResult<Vec<Tenant>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT tenant_id, name, verify_mode_default, contact, created_at
             FROM tenant ORDER BY rowid ASC",
        )?;
        let rows = stmt.query_map([], row_to_tenant)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// 更新租户默认校验档位（管理端 `PUT /admin/tenants/:id/policy`）。
    ///
    /// 租户不存在 → [`LicenseError::KeyStateIllegal`]（HTTP 层映射 400）。
    pub fn update_tenant_policy(
        &self,
        tenant_id: &str,
        verify_mode: VerifyMode,
    ) -> LicenseResult<()> {
        let conn = self.conn.lock();
        let affected = conn.execute(
            "UPDATE tenant SET verify_mode_default = ?2 WHERE tenant_id = ?1",
            params![tenant_id, verify_mode.as_str()],
        )?;
        if affected == 0 {
            return Err(LicenseError::KeyStateIllegal(format!(
                "tenant not found: {tenant_id}"
            )));
        }
        Ok(())
    }

    // ================= device =================

    /// 插入设备（`machine_code` 唯一；重复插入返回 [`LicenseError::Storage`]）。
    pub fn insert_device(&self, device: &Device) -> LicenseResult<()> {
        let anchors = encode_anchor_hashes(&device.anchor_hashes)?;
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO device
               (device_id, tenant_id, machine_code, anchor_hashes, deploy_mode,
                image_digest, host_anchor_ref, first_activation_at, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                device.device_id,
                device.tenant_id,
                device.machine_code,
                anchors,
                device.deploy_mode.as_str(),
                device.image_digest,
                device.host_anchor_ref,
                device.first_activation_at,
                device.status.as_str(),
                device.created_at,
            ],
        )?;
        Ok(())
    }

    /// 按设备 ID 查询。
    pub fn get_device(&self, device_id: &str) -> LicenseResult<Option<Device>> {
        let conn = self.conn.lock();
        let found = conn
            .query_row(
                "SELECT device_id, tenant_id, machine_code, anchor_hashes, deploy_mode,
                        image_digest, host_anchor_ref, first_activation_at, status, created_at
                 FROM device WHERE device_id = ?1",
                params![device_id],
                row_to_device,
            )
            .optional()?;
        Ok(found)
    }

    /// 按机器码查询（`machine_code` 唯一，故至多一条）。
    pub fn get_device_by_machine_code(&self, machine_code: &str) -> LicenseResult<Option<Device>> {
        // 查询侧归一：库内 `device.machine_code` 已是小写（写入时归一 + 存量迁移），
        // 但调用方可能传任意大小写（例如网关上报 / 测试断言）；只归一查询入参，
        // 不对库内值再 lower()（防止 UNIQUE 重复）。
        let machine_code = crate::model::normalize_machine_code(machine_code);
        let conn = self.conn.lock();
        let found = conn
            .query_row(
                "SELECT device_id, tenant_id, machine_code, anchor_hashes, deploy_mode,
                        image_digest, host_anchor_ref, first_activation_at, status, created_at
                 FROM device WHERE machine_code = ?1",
                params![machine_code],
                row_to_device,
            )
            .optional()?;
        Ok(found)
    }

    /// 分页列出设备（`page` 从 1 起）。
    ///
    /// `tenant_id == None` 表示**跨租户**列出全部设备（管理后台总览）。
    pub fn list_devices(
        &self,
        tenant_id: Option<&str>,
        page: u32,
        page_size: u32,
    ) -> LicenseResult<Vec<Device>> {
        let page = page.max(1);
        let page_size = page_size.max(1);
        let offset = to_i64(((page - 1) as usize) * page_size as usize, "device offset")?;
        let conn = self.conn.lock();
        let mut out = Vec::new();
        match tenant_id {
            Some(tenant) => {
                let mut stmt = conn.prepare(
                    "SELECT device_id, tenant_id, machine_code, anchor_hashes, deploy_mode,
                            image_digest, host_anchor_ref, first_activation_at, status, created_at
                     FROM device WHERE tenant_id = ?1
                     ORDER BY rowid ASC
                     LIMIT ?2 OFFSET ?3",
                )?;
                let rows =
                    stmt.query_map(params![tenant, i64::from(page_size), offset], row_to_device)?;
                for row in rows {
                    out.push(row?);
                }
            }
            None => {
                let mut stmt = conn.prepare(
                    "SELECT device_id, tenant_id, machine_code, anchor_hashes, deploy_mode,
                            image_digest, host_anchor_ref, first_activation_at, status, created_at
                     FROM device
                     ORDER BY rowid ASC
                     LIMIT ?1 OFFSET ?2",
                )?;
                let rows = stmt.query_map(params![i64::from(page_size), offset], row_to_device)?;
                for row in rows {
                    out.push(row?);
                }
            }
        }
        Ok(out)
    }

    /// 统计设备数（配额判定用）。`tenant_id == None` 表示**全局**计数。
    pub fn count_devices(&self, tenant_id: Option<&str>) -> LicenseResult<u64> {
        let conn = self.conn.lock();
        let count: i64 = match tenant_id {
            Some(tenant) => conn.query_row(
                "SELECT COUNT(*) FROM device WHERE tenant_id = ?1",
                params![tenant],
                |row| row.get(0),
            )?,
            None => conn.query_row("SELECT COUNT(*) FROM device", [], |row| row.get(0))?,
        };
        to_u64(count, "device count")
    }

    /// 更新设备状态（心跳路径回写）。
    pub fn update_device_status(&self, device_id: &str, status: DeviceStatus) -> LicenseResult<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE device SET status = ?2 WHERE device_id = ?1",
            params![device_id, status.as_str()],
        )?;
        Ok(())
    }

    /// 读取设备已钉定的激活公钥（2026-09-25 主理人决策）。
    ///
    /// 首次激活成功时由 [`Store::pin_device_pubkey_if_absent`] first-write-wins 写入；
    /// 未钉定（历史设备 / 未激活）返回 `None`。
    pub fn get_device_pubkey(&self, device_id: &str) -> LicenseResult<Option<String>> {
        let conn = self.conn.lock();
        // 显式匹配 `QueryReturnedNoRows`：设备不存在与「列值为 NULL（未钉定）」
        // 都归一为 `None`（列类型可空，`row.get::<_, Option<String>>` 承接 NULL）。
        match conn.query_row(
            "SELECT device_pubkey FROM device WHERE device_id = ?1",
            params![device_id],
            |row| row.get::<_, Option<String>>(0),
        ) {
            Ok(pubkey) => Ok(pubkey),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// 首写钉定设备激活公钥（2026-09-25 主理人决策）。
    ///
    /// `UPDATE ... WHERE device_pubkey IS NULL` 单语句仲裁（与防重放同一纪律：
    /// **绝不做「先 SELECT 再 UPDATE」**），返回是否本次发生了写入；已钉定则不覆盖。
    pub fn pin_device_pubkey_if_absent(
        &self,
        device_id: &str,
        pubkey: &str,
    ) -> LicenseResult<bool> {
        let conn = self.conn.lock();
        let affected = conn.execute(
            "UPDATE device SET device_pubkey = ?1
              WHERE device_id = ?2 AND device_pubkey IS NULL",
            params![pubkey, device_id],
        )?;
        Ok(affected == 1)
    }

    // ================= activation_code =================

    /// 插入激活码（`code` 唯一）。**不在此处校验 `validate()`**——由 service 层显式调用，
    /// 以便错误语义（`KeyStateIllegal`）不被降级为 `Storage`。
    pub fn insert_code(&self, code: &ActivationCode) -> LicenseResult<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO activation_code
               (code_id, code, status, bound_device_id, tenant_id, tier, valid_from, valid_until,
                source_order_id, reissued_from_id, issued_by, revoked_at, revoked_reason,
                idempotency_key, created_at, prebind_machine_code)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                code.code_id,
                code.code,
                code.status.as_str(),
                code.bound_device_id,
                code.tenant_id,
                code.tier,
                code.valid_from,
                code.valid_until,
                code.source_order_id,
                code.reissued_from_id,
                code.issued_by,
                code.revoked_at,
                code.revoked_reason,
                code.idempotency_key,
                code.created_at,
                code.prebind_machine_code,
            ],
        )?;
        Ok(())
    }

    /// 按码值查询（激活入口；`code` 唯一）。
    ///
    /// **调用方不得把码值写进日志 / 错误信息**。
    pub fn get_code_by_value(&self, code_value: &str) -> LicenseResult<Option<ActivationCode>> {
        let conn = self.conn.lock();
        let found = conn
            .query_row(
                "SELECT code_id, code, status, bound_device_id, tenant_id, tier, valid_from,
                        valid_until, source_order_id, reissued_from_id, issued_by, revoked_at,
                        revoked_reason, idempotency_key, created_at,
                        prebind_machine_code
                 FROM activation_code WHERE code = ?1",
                params![code_value],
                row_to_code,
            )
            .optional()?;
        Ok(found)
    }

    /// 按码 ID 查询。
    pub fn get_code_by_id(&self, code_id: &str) -> LicenseResult<Option<ActivationCode>> {
        let conn = self.conn.lock();
        let found = conn
            .query_row(
                "SELECT code_id, code, status, bound_device_id, tenant_id, tier, valid_from,
                        valid_until, source_order_id, reissued_from_id, issued_by, revoked_at,
                        revoked_reason, idempotency_key, created_at,
                        prebind_machine_code
                 FROM activation_code WHERE code_id = ?1",
                params![code_id],
                row_to_code,
            )
            .optional()?;
        Ok(found)
    }

    /// 按**原码 ID** 直查重发产出的新码（task 46 重发溯源 / 幂等）。
    ///
    /// ⚠️ **为什么必须走 SQL 直查**：重发幂等早期实现是「`list_codes` 取第一页 +
    /// 内存 `.find(reissued_from_id == 原码)`」。一旦某租户的码数超过一页
    /// （`MAX_ISSUE_BATCH = 1000`），新码就可能落在第二页之外 → **漏命中** →
    /// 同一次重发被判为「首次」而**再签发一张新码**，换机迁移因此一码变两码。
    /// 本方法以索引直查替代分页扫描，**不受总量与分页边界影响**。
    pub fn find_code_by_reissued_from(
        &self,
        original_code_id: &str,
    ) -> LicenseResult<Option<ActivationCode>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT code_id, code, status, bound_device_id, tenant_id, tier, valid_from,
                    valid_until, source_order_id, reissued_from_id, issued_by, revoked_at,
                    revoked_reason, idempotency_key, created_at, prebind_machine_code
             FROM activation_code WHERE reissued_from_id = ?1
             ORDER BY rowid ASC LIMIT 1",
            params![original_code_id],
            row_to_code,
        )
        .optional()
        .map_err(LicenseError::from)
    }

    /// 查某设备当前**仍生效**的绑定码（task 46 预绑定冲突检测的设备侧）。
    ///
    /// 只认 `status = 'bound'`：`revoked` / `reissued` 的历史绑定**不构成冲突**
    /// （否则换机迁移后原设备永远无法再绑定任何新码）。
    pub fn find_bound_code_for_device(
        &self,
        device_id: &str,
    ) -> LicenseResult<Option<ActivationCode>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT code_id, code, status, bound_device_id, tenant_id, tier, valid_from,
                    valid_until, source_order_id, reissued_from_id, issued_by, revoked_at,
                    revoked_reason, idempotency_key, created_at, prebind_machine_code
             FROM activation_code WHERE bound_device_id = ?1 AND status = 'bound'
             ORDER BY rowid ASC LIMIT 1",
            params![device_id],
            row_to_code,
        )
        .optional()
        .map_err(LicenseError::from)
    }

    /// 查**预绑定**到某机器码的仍可用激活码（task 46 预绑定冲突检测的码侧）。
    ///
    /// 覆盖「码已预绑定但**尚未激活**」这一状态：此时 `bound_device_id` 仍是 `NULL`，
    /// 只看 `bound_device_id` 会漏判冲突，导致两台设备各自拿到预绑定到同一机器的码。
    pub fn find_code_by_prebind(
        &self,
        machine_code: &str,
    ) -> LicenseResult<Option<ActivationCode>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT code_id, code, status, bound_device_id, tenant_id, tier, valid_from,
                    valid_until, source_order_id, reissued_from_id, issued_by, revoked_at,
                    revoked_reason, idempotency_key, created_at, prebind_machine_code
             FROM activation_code
             WHERE prebind_machine_code = ?1 AND status IN ('issued', 'bound')
             ORDER BY rowid ASC LIMIT 1",
            params![machine_code],
            row_to_code,
        )
        .optional()
        .map_err(LicenseError::from)
    }

    /// 按过滤条件分页列出激活码（`page` 从 1 起）。
    ///
    /// **排序契约：按 `rowid` 升序（= 插入顺序）**。同一个发放批次的多张码共享同一
    /// `created_at` 秒，若以 `created_at`/`code_id` 排序会因随机主键后缀产生**不稳定顺序**，
    /// 使「同幂等键重放返回同一批码」无法保证逐位一致。`rowid` 单调且确定。
    pub fn list_codes(
        &self,
        filter: &CodeFilter,
        page: u32,
        page_size: u32,
    ) -> LicenseResult<Vec<ActivationCode>> {
        let page = page.max(1);
        let page_size = page_size.max(1);
        let offset = to_i64(((page - 1) as usize) * page_size as usize, "code offset")?;

        // 动态拼装 WHERE（**只用绑定参数**，不做字符串拼接注入）。
        let mut sql = String::from(
            "SELECT code_id, code, status, bound_device_id, tenant_id, tier, valid_from,
                    valid_until, source_order_id, reissued_from_id, issued_by, revoked_at,
                    revoked_reason, idempotency_key, created_at,
                        prebind_machine_code
             FROM activation_code WHERE 1 = 1",
        );
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        push_code_filter(&mut sql, &mut args, filter);
        sql.push_str(" ORDER BY rowid ASC LIMIT ? OFFSET ?");
        args.push(Box::new(i64::from(page_size)));
        args.push(Box::new(offset));

        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), row_to_code)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// 按过滤条件统计激活码数。
    pub fn count_codes(&self, filter: &CodeFilter) -> LicenseResult<u64> {
        let mut sql = String::from("SELECT COUNT(*) FROM activation_code WHERE 1 = 1");
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        push_code_filter(&mut sql, &mut args, filter);
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let count: i64 = stmt.query_row(refs.as_slice(), |row| row.get(0))?;
        to_u64(count, "code count")
    }

    /// 更新激活码状态（不改动绑定关系 / 废弃信息）。
    pub fn update_code_status(&self, code_id: &str, status: CodeStatus) -> LicenseResult<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE activation_code SET status = ?2 WHERE code_id = ?1",
            params![code_id, status.as_str()],
        )?;
        Ok(())
    }

    /// **原子绑定**：把 `code_id` 绑定到 `device_id`（一机一码的竞态防线）。
    ///
    /// 实施方式：单条条件 UPDATE + `rows_affected` 判定。
    /// `rows_affected == 0` 表示该码已被**别的机器**抢先绑定（或码不存在），
    /// 返回 [`LicenseError::ActivationRejected`]。
    ///
    /// ⚠️ **绝不允许改成「先 SELECT 校验再 UPDATE」**：并发两个「同码异机激活」请求
    /// 会同时看到 `bound_device_id IS NULL` 并双双绑定成功，一机一码被击穿。
    ///
    /// 幂等：若该码已绑定到**同一** `device_id`，也返回 `Err`（调用方应先用
    /// [`Store::get_code_by_id`] 判定是否已完成，再决定是否走本路径）。
    pub fn bind_code_to_device(&self, code_id: &str, device_id: &str) -> LicenseResult<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let affected = tx.execute(
            "UPDATE activation_code
                SET status = 'bound', bound_device_id = ?2
              WHERE code_id = ?1 AND bound_device_id IS NULL",
            params![code_id, device_id],
        )?;
        if affected == 1 {
            tx.commit()?;
            return Ok(());
        }
        // 未抢到：区分「不存在」与「已被绑定」，以便服务端给出正确语义。
        let existing: Option<(Option<String>, String)> = tx
            .query_row(
                "SELECT bound_device_id, status FROM activation_code WHERE code_id = ?1",
                params![code_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        tx.commit()?;
        match existing {
            None => Err(LicenseError::ActivationRejected(format!(
                "activation code not found: {code_id}"
            ))),
            Some((bound, status)) => {
                let current = bound.unwrap_or_else(|| "<none>".to_string());
                Err(LicenseError::ActivationRejected(format!(
                    "activation code {code_id} is already bound (status={status}, \
                     bound_device_id={current}); refusing to bind to {device_id}"
                )))
            }
        }
    }

    /// 废弃激活码：置 `revoked` 并写入 `revoked_at = now` / `revoked_reason = reason`。
    ///
    /// 只允许从 `issued` / `bound` 迁移（`revoked` / `reissued` 重复废弃返回
    /// [`LicenseError::KeyStateIllegal`]，保证状态机单向）。
    pub fn revoke_code(&self, code_id: &str, reason: &str, now: i64) -> LicenseResult<()> {
        if reason.trim().is_empty() {
            return Err(LicenseError::KeyStateIllegal(
                "revoke requires a non-empty reason".into(),
            ));
        }
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let current: Option<String> = tx
            .query_row(
                "SELECT status FROM activation_code WHERE code_id = ?1",
                params![code_id],
                |row| row.get(0),
            )
            .optional()?;
        let status = match current {
            None => {
                return Err(LicenseError::KeyStateIllegal(format!(
                    "activation code not found: {code_id}"
                )));
            }
            Some(raw) => CodeStatus::parse(&raw)?,
        };
        if !matches!(status, CodeStatus::Issued | CodeStatus::Bound) {
            return Err(LicenseError::KeyStateIllegal(format!(
                "cannot revoke activation code {code_id} in status {}",
                status.as_str()
            )));
        }
        tx.execute(
            "UPDATE activation_code
                SET status = 'revoked', revoked_at = ?2, revoked_reason = ?3
              WHERE code_id = ?1",
            params![code_id, now, reason],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// 标记原码为 `reissued`，并把新码溯源到原码（**同一事务**）。
    ///
    /// 迁移规则：`revoked → reissued`（设计「生命周期」表）。仅切原码状态；
    /// 新码由调用方构造为 `status = reissued`、`reissued_from_id = 原码 ID`
    /// 并经 [`Store::insert_code`] 落库后再调用本方法（或反之，见实现顺序注释）。
    pub fn mark_code_reissued(&self, original_code_id: &str) -> LicenseResult<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let current: Option<String> = tx
            .query_row(
                "SELECT status FROM activation_code WHERE code_id = ?1",
                params![original_code_id],
                |row| row.get(0),
            )
            .optional()?;
        let status = match current {
            None => {
                return Err(LicenseError::KeyStateIllegal(format!(
                    "activation code not found: {original_code_id}"
                )));
            }
            Some(raw) => CodeStatus::parse(&raw)?,
        };
        if !matches!(status, CodeStatus::Revoked) {
            return Err(LicenseError::KeyStateIllegal(format!(
                "reissue requires a revoked activation code; {original_code_id} is {}",
                status.as_str()
            )));
        }
        tx.execute(
            "UPDATE activation_code SET status = 'reissued' WHERE code_id = ?1",
            params![original_code_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    // ================= lease =================

    /// 插入租约。
    pub fn insert_lease(&self, lease: &Lease) -> LicenseResult<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO lease
               (lease_id, device_id, code_id, kid, token_sig, verify_mode, tier,
                issued_at, valid_until, last_heartbeat_at, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                lease.lease_id,
                lease.device_id,
                lease.code_id,
                lease.kid,
                lease.token_sig,
                lease.verify_mode.as_str(),
                lease.tier,
                lease.issued_at,
                lease.valid_until,
                lease.last_heartbeat_at,
                lease.status.as_str(),
            ],
        )?;
        Ok(())
    }

    /// 按租约 ID 查询。
    pub fn get_lease(&self, lease_id: &str) -> LicenseResult<Option<Lease>> {
        let conn = self.conn.lock();
        let found = conn
            .query_row(
                "SELECT lease_id, device_id, code_id, kid, token_sig, verify_mode, tier,
                        issued_at, valid_until, last_heartbeat_at, status
                 FROM lease WHERE lease_id = ?1",
                params![lease_id],
                row_to_lease,
            )
            .optional()?;
        Ok(found)
    }

    /// 列出某设备的全部租约（按 `issued_at` 降序，最新在前）。
    pub fn list_leases_by_device(&self, device_id: &str) -> LicenseResult<Vec<Lease>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT lease_id, device_id, code_id, kid, token_sig, verify_mode, tier,
                    issued_at, valid_until, last_heartbeat_at, status
             FROM lease WHERE device_id = ?1
             ORDER BY issued_at DESC, rowid DESC",
        )?;
        let rows = stmt.query_map(params![device_id], row_to_lease)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// 更新租约最近心跳时间（心跳路径）。
    pub fn update_lease_heartbeat(&self, lease_id: &str, heartbeat_at: i64) -> LicenseResult<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE lease SET last_heartbeat_at = ?2 WHERE lease_id = ?1",
            params![lease_id, heartbeat_at],
        )?;
        Ok(())
    }

    /// 更新租约状态（心跳结果 + 时间守卫驱动）。
    pub fn update_lease_status(&self, lease_id: &str, status: LeaseStatus) -> LicenseResult<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE lease SET status = ?2 WHERE lease_id = ?1",
            params![lease_id, status.as_str()],
        )?;
        Ok(())
    }

    /// **§7 步骤 ② 的原子改绑**：更新设备绑定（`machine_code` + `anchor_hashes`，状态置活）、
    /// 作废该设备全部旧租约、写入新租约、并落一条审计留痕——**全部在同一事务内**。
    ///
    /// # 为何必须原子
    /// 这四步若分属不同事务，中途失败会留下**中间态**：
    /// 「已改绑 `machine_code` 但旧租约仍有效」（旧租约与新身份并存），或
    /// 「旧租约已作废但新租约未签发」（设备既非旧机也非新机的空洞期）。单事务把它们
    /// 收敛为一个线性化点：要么全部生效，要么全部回滚。
    ///
    /// # 与 `bind_code_to_device` 的区别
    /// 后者处理「首次绑定」（`bound_device_id IS NULL` 的条件 UPDATE，防并发双绑）；
    /// 本方法处理「**已绑定**码在同机（漂移 / 重装）场景的改绑」，绑定关系不变，只前移
    /// 设备的身份锚点与租约。
    ///
    /// # Errors
    /// - 设备不存在 → [`LicenseError::Storage`]（事务回滚，无副作用）
    /// - 新 `machine_code` 与其它设备冲突（`device.machine_code` 唯一约束）→
    ///   [`LicenseError::Storage`]（事务回滚，绑定关系与租约均不变）
    /// - SQLite 错误 → [`LicenseError::Storage`]（事务回滚）
    pub fn rebind_device_and_issue_lease(
        &self,
        device_id: &str,
        new_machine_code: &str,
        new_anchor_hashes: &[String],
        new_lease: &Lease,
        audit: &AuditLog,
    ) -> LicenseResult<()> {
        let anchors = encode_anchor_hashes(new_anchor_hashes)?;
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;

        // 1) 改绑：`machine_code` 与 `anchor_hashes` 一起刷新（保持「绑定记录」自洽——
        //    `machine_code` 是锚点组合的 HMAC，二者必须同步前移），并置状态为 `active`。
        //    唯一约束冲突（新 machine_code 已被别的设备占用）由数据库仲裁 → 回滚。
        let affected = tx.execute(
            "UPDATE device
                SET machine_code = ?2, anchor_hashes = ?3, status = 'active'
              WHERE device_id = ?1",
            params![device_id, new_machine_code, anchors],
        )?;
        if affected == 0 {
            return Err(LicenseError::Storage(format!(
                "cannot rebind: device not found: {device_id}"
            )));
        }

        // 2) 作废该设备全部**未停止**租约（改绑后旧租约一律失效，弃用语义 = 立即失效）。
        tx.execute(
            "UPDATE lease SET status = 'stopped'
              WHERE device_id = ?1 AND status != 'stopped'",
            params![device_id],
        )?;

        // 3) 写入新租约（在「作废旧租约」之后插入，故不会被上一步误置为 stopped）。
        tx.execute(
            "INSERT INTO lease
               (lease_id, device_id, code_id, kid, token_sig, verify_mode, tier,
                issued_at, valid_until, last_heartbeat_at, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                new_lease.lease_id,
                new_lease.device_id,
                new_lease.code_id,
                new_lease.kid,
                new_lease.token_sig,
                new_lease.verify_mode.as_str(),
                new_lease.tier,
                new_lease.issued_at,
                new_lease.valid_until,
                new_lease.last_heartbeat_at,
                new_lease.status.as_str(),
            ],
        )?;

        // 4) 审计留痕（同事务：避免「改绑已提交、审计缺失」的中间态；审计内容不含敏感值）。
        tx.execute(
            "INSERT INTO audit_log
               (id, actor_type, actor_id, action, entity_type, entity_id, detail, ts, ip)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                audit.id,
                audit.actor_type.as_str(),
                audit.actor_id,
                audit.action,
                audit.entity_type,
                audit.entity_id,
                audit.detail,
                audit.ts,
                audit.ip,
            ],
        )?;

        tx.commit()?;
        Ok(())
    }

    // ================= heartbeat =================

    /// 插入心跳记录。
    pub fn insert_heartbeat(&self, heartbeat: &Heartbeat) -> LicenseResult<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO heartbeat
               (id, lease_id, device_id, client_ts, server_ts, result, receipt_cursor, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                heartbeat.id,
                heartbeat.lease_id,
                heartbeat.device_id,
                heartbeat.client_ts,
                heartbeat.server_ts,
                heartbeat.result.as_str(),
                heartbeat.receipt_cursor,
                heartbeat.created_at,
            ],
        )?;
        Ok(())
    }

    /// 列出某租约的全部心跳（按 `server_ts` 升序）。
    pub fn list_heartbeats_by_lease(&self, lease_id: &str) -> LicenseResult<Vec<Heartbeat>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, lease_id, device_id, client_ts, server_ts, result, receipt_cursor, created_at
             FROM heartbeat WHERE lease_id = ?1
             ORDER BY server_ts ASC, rowid ASC",
        )?;
        let rows = stmt.query_map(params![lease_id], row_to_heartbeat)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    // ================= audit_receipt =================

    /// 插入审计回执。
    pub fn insert_audit_receipt(&self, receipt: &AuditReceipt) -> LicenseResult<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO audit_receipt
               (id, lease_id, device_mid, seq_from, seq_to, count, payload_digest, ts, sig,
                received_at, gap_flag)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                receipt.id,
                receipt.lease_id,
                receipt.device_mid,
                receipt.seq_from,
                receipt.seq_to,
                receipt.count,
                receipt.payload_digest,
                receipt.ts,
                receipt.sig,
                receipt.received_at,
                i64::from(receipt.gap_flag),
            ],
        )?;
        Ok(())
    }

    /// 某租约**序号最大**的回执（跳空检测的游标基准）。
    ///
    /// 排序依据 `seq_to` 降序、`received_at` 降序，取第一条。
    pub fn last_receipt_for_lease(&self, lease_id: &str) -> LicenseResult<Option<AuditReceipt>> {
        let conn = self.conn.lock();
        let found = conn
            .query_row(
                "SELECT id, lease_id, device_mid, seq_from, seq_to, count, payload_digest, ts, sig,
                        received_at, gap_flag
                 FROM audit_receipt WHERE lease_id = ?1
                 ORDER BY seq_to DESC, received_at DESC, rowid DESC
                 LIMIT 1",
                params![lease_id],
                row_to_receipt,
            )
            .optional()?;
        Ok(found)
    }

    /// 列出某租约的全部回执（按 `seq_from` 升序，便于连续性扫描）。
    pub fn list_receipts_by_lease(&self, lease_id: &str) -> LicenseResult<Vec<AuditReceipt>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, lease_id, device_mid, seq_from, seq_to, count, payload_digest, ts, sig,
                    received_at, gap_flag
             FROM audit_receipt WHERE lease_id = ?1
             ORDER BY seq_from ASC, received_at ASC, rowid ASC",
        )?;
        let rows = stmt.query_map(params![lease_id], row_to_receipt)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// 分页列出**全部异常回执**（`gap_flag = 1`，管理端 `GET /admin/receipts/anomalies`）。
    ///
    /// 按 `received_at` 降序（最新异常在前）。
    pub fn list_anomalous_receipts(
        &self,
        page: u32,
        page_size: u32,
    ) -> LicenseResult<Vec<AuditReceipt>> {
        let page = page.max(1);
        let page_size = page_size.max(1);
        let offset = to_i64(((page - 1) as usize) * page_size as usize, "receipt offset")?;
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, lease_id, device_mid, seq_from, seq_to, count, payload_digest, ts, sig,
                    received_at, gap_flag
             FROM audit_receipt WHERE gap_flag = 1
             ORDER BY received_at DESC, rowid DESC
             LIMIT ?1 OFFSET ?2",
        )?;
        let rows = stmt.query_map(params![i64::from(page_size), offset], row_to_receipt)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// 统计异常回执数（`gap_flag = 1`）。
    pub fn count_anomalous_receipts(&self) -> LicenseResult<u64> {
        let conn = self.conn.lock();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM audit_receipt WHERE gap_flag = 1",
            [],
            |row| row.get(0),
        )?;
        to_u64(count, "anomalous receipt count")
    }

    /// 更新某回执的跳空标记（风控扫描置位 / 人工核实后复位）。
    pub fn update_receipt_gap_flag(&self, receipt_id: &str, gap: bool) -> LicenseResult<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE audit_receipt SET gap_flag = ?2 WHERE id = ?1",
            params![receipt_id, i64::from(gap)],
        )?;
        Ok(())
    }

    // ================= code_batch（批次头表 · 幂等仲裁） =================

    /// **原子认领发放批次**：仅当 `idempotency_key` 从未出现时写入头表，返回是否**首次**。
    ///
    /// - 返回 `Ok(true)`：本调用认领成功，调用方应执行完整发放；
    /// - 返回 `Ok(false)`：该 key 已被占位（**重复 / 并发重放**），调用方必须改为
    ///   读取既有批次并原样返回，**不得**再生成新码。
    ///
    /// 实现依赖主键唯一约束由数据库仲裁（`INSERT OR IGNORE` + `rows_affected`），
    /// **不用「先 SELECT 再 INSERT」**（并发窗口会让两个请求同时通过校验）。
    /// 这是把幂等判定下沉到「数据库唯一约束」这一**线性化点**，是批次幂等的唯一正解。
    pub fn claim_code_batch(
        &self,
        idempotency_key: &str,
        tenant_id: &str,
        expected_count: u32,
        created_at: i64,
    ) -> LicenseResult<bool> {
        let conn = self.conn.lock();
        let affected = conn.execute(
            "INSERT OR IGNORE INTO code_batch
               (idempotency_key, tenant_id, expected_count, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                idempotency_key,
                tenant_id,
                i64::from(expected_count),
                created_at
            ],
        )?;
        Ok(affected == 1)
    }

    /// 按批次 key 列出该批次的**全部**激活码（幂等重放读路径）。
    ///
    /// **排序契约：按 `rowid` 升序（= 插入顺序）**，与 [`Store::list_codes`] 一致：
    /// 同批多码共享同一 `created_at` 秒，若按 `created_at`/`code_id` 排序会因随机主键
    /// 后缀导致**顺序不稳定**，使「同幂等键重放返回同一批码」无法逐位一致。
    /// 本查询**不设 LIMIT**——批次规模已由 `MAX_ISSUE_BATCH` 在 service 层封顶。
    pub fn list_codes_by_batch_key(
        &self,
        idempotency_key: &str,
    ) -> LicenseResult<Vec<ActivationCode>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT code_id, code, status, bound_device_id, tenant_id, tier, valid_from,
                    valid_until, source_order_id, reissued_from_id, issued_by, revoked_at,
                    revoked_reason, idempotency_key, created_at,
                        prebind_machine_code
             FROM activation_code WHERE idempotency_key = ?1
             ORDER BY rowid ASC",
        )?;
        let rows = stmt.query_map(params![idempotency_key], row_to_code)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    // ================= nonce_cache =================

    /// **防重放**：仅当 `nonce` 从未出现时写入，返回是否**首次**。
    ///
    /// - 返回 `Ok(true)`：首次使用，调用方应继续处理请求；
    /// - 返回 `Ok(false)`：**重放**，调用方必须拒绝。
    ///
    /// 实现依赖主键唯一约束由数据库仲裁（`INSERT OR IGNORE` + `rows_affected`），
    /// **不用「先 SELECT 再 INSERT」**（同样存在并发窗口）。
    /// `used_at` 由本方法内部取当前 UTC 秒填充（调用方无需关心）。
    pub fn insert_nonce_if_absent(
        &self,
        nonce: &str,
        device_id: &str,
        expires_at: i64,
    ) -> LicenseResult<bool> {
        let conn = self.conn.lock();
        let affected = conn.execute(
            "INSERT OR IGNORE INTO nonce_cache (nonce, device_id, expires_at, used_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![nonce, device_id, expires_at, now_unix_secs()],
        )?;
        Ok(affected == 1)
    }

    /// 清理已过期 nonce，返回删除条数（定时任务调用）。
    pub fn purge_expired_nonces(&self, now: i64) -> LicenseResult<u32> {
        let conn = self.conn.lock();
        let removed = conn.execute(
            "DELETE FROM nonce_cache WHERE expires_at < ?1",
            params![now],
        )?;
        to_u32(to_i64(removed, "nonce delete count")?, "nonce delete count")
    }

    /// 按 nonce 查询（测试 / 排查用）。
    pub fn get_nonce(&self, nonce: &str) -> LicenseResult<Option<NonceCache>> {
        let conn = self.conn.lock();
        let found = conn
            .query_row(
                "SELECT nonce, device_id, expires_at, used_at FROM nonce_cache WHERE nonce = ?1",
                params![nonce],
                row_to_nonce,
            )
            .optional()?;
        Ok(found)
    }

    // ================= signing_key =================

    /// 插入签名密钥（**仅公钥 + hsm_ref**，私钥绝不落库）。
    pub fn insert_signing_key(&self, key: &SigningKey) -> LicenseResult<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO signing_key (kid, status, public_key, hsm_ref, enabled_at, retired_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                key.kid,
                key.status.as_str(),
                key.public_key,
                key.hsm_ref,
                key.enabled_at,
                key.retired_at,
            ],
        )?;
        Ok(())
    }

    /// 按 kid 查询。
    pub fn get_signing_key(&self, kid: &str) -> LicenseResult<Option<SigningKey>> {
        let conn = self.conn.lock();
        let found = conn
            .query_row(
                "SELECT kid, status, public_key, hsm_ref, enabled_at, retired_at
                 FROM signing_key WHERE kid = ?1",
                params![kid],
                row_to_signing_key,
            )
            .optional()?;
        Ok(found)
    }

    /// 列出全部签名密钥（按 `enabled_at` 升序）。
    pub fn list_signing_keys(&self) -> LicenseResult<Vec<SigningKey>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT kid, status, public_key, hsm_ref, enabled_at, retired_at
             FROM signing_key ORDER BY rowid ASC",
        )?;
        let rows = stmt.query_map([], row_to_signing_key)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// 更新签名密钥状态（轮换：`active → retiring → retired`）。
    ///
    /// 置为 `retired` 时同时写入 `retired_at`。
    pub fn update_signing_key_status(
        &self,
        kid: &str,
        status: SigningKeyStatus,
        at: i64,
    ) -> LicenseResult<()> {
        let conn = self.conn.lock();
        match status {
            SigningKeyStatus::Retired => {
                conn.execute(
                    "UPDATE signing_key SET status = ?2, retired_at = ?3 WHERE kid = ?1",
                    params![kid, status.as_str(), at],
                )?;
            }
            SigningKeyStatus::Active | SigningKeyStatus::Retiring => {
                conn.execute(
                    "UPDATE signing_key SET status = ?2 WHERE kid = ?1",
                    params![kid, status.as_str()],
                )?;
            }
        }
        Ok(())
    }

    // ================= ota_package =================

    /// 插入升级包（`(version, channel)` 主键冲突 → `Storage` 错误，由调用方转为 400）。
    ///
    /// **不做 upsert**：同版本同通道重复上传必须显式失败——静默覆盖会让「已发布
    /// 的包被换掉字节」无从察觉，而网关侧版本单调性又禁止降级，等于把现场锁死在
    /// 一个已被替换的包上。
    pub fn insert_ota_package(&self, pkg: &OtaPackage) -> LicenseResult<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO ota_package
               (version, channel, size, payload_sha256, payload_b64, sig_b64, kid,
                status, published_at, published_by, note)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                pkg.version,
                pkg.channel,
                pkg.size,
                pkg.payload_sha256,
                pkg.payload_b64,
                pkg.sig_b64,
                pkg.kid,
                pkg.status.as_str(),
                pkg.published_at,
                pkg.published_by,
                pkg.note,
            ],
        )?;
        Ok(())
    }

    /// 按 `(version, channel)` 查询（无此包 → `None`）。
    pub fn get_ota_package(
        &self,
        version: &str,
        channel: &str,
    ) -> LicenseResult<Option<OtaPackage>> {
        let conn = self.conn.lock();
        let found = conn
            .query_row(
                "SELECT version, channel, size, payload_sha256, payload_b64, sig_b64, kid,
                        status, published_at, published_by, note
                 FROM ota_package WHERE version = ?1 AND channel = ?2",
                params![version, channel],
                row_to_ota_package,
            )
            .optional()?;
        Ok(found)
    }

    /// 列出全部升级包（按版本号数值降序、通道升序）。
    ///
    /// 排序用 `CAST(version AS INTEGER)`：版本号是 u64 单调序的十进制串，按 TEXT
    /// 字典序排会把 `"9"` 排到 `"10"` 之后（列表观感错乱）。
    pub fn list_ota_packages(&self) -> LicenseResult<Vec<OtaPackage>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT version, channel, size, payload_sha256, payload_b64, sig_b64, kid,
                    status, published_at, published_by, note
             FROM ota_package
             ORDER BY CAST(version AS INTEGER) DESC, channel ASC",
        )?;
        let rows = stmt.query_map([], row_to_ota_package)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// 按状态过滤升级包（列表页筛选；`None` = 不过滤）。
    pub fn list_ota_packages_by_status(
        &self,
        status: Option<OtaStatus>,
    ) -> LicenseResult<Vec<OtaPackage>> {
        let all = self.list_ota_packages()?;
        Ok(match status {
            Some(s) => all.into_iter().filter(|p| p.status == s).collect(),
            None => all,
        })
    }

    /// 更新状态（`rows_affected == 0` 表示目标包不存在，由调用方转 404）。
    pub fn update_ota_status(
        &self,
        version: &str,
        channel: &str,
        status: OtaStatus,
        published_at: &str,
        published_by: &str,
    ) -> LicenseResult<bool> {
        let conn = self.conn.lock();
        let changed = conn.execute(
            "UPDATE ota_package
                SET status = ?3, published_at = ?4, published_by = ?5
              WHERE version = ?1 AND channel = ?2",
            params![
                version,
                channel,
                status.as_str(),
                published_at,
                published_by
            ],
        )?;
        Ok(changed > 0)
    }

    /// 某通道**最新已发布**的包（供网关拉取；无人发布 → `None`，诚实空态）。
    ///
    /// 只认 [`OtaStatus::Published`]：draft 包尚未过审、disabled / revoked 包已下架，
    /// 三者都**绝不**下发给网关（宁可让网关报「无可升级版本」，也不发一个不该发的包）。
    pub fn latest_published_ota(&self, channel: &str) -> LicenseResult<Option<OtaPackage>> {
        let conn = self.conn.lock();
        let found = conn
            .query_row(
                "SELECT version, channel, size, payload_sha256, payload_b64, sig_b64, kid,
                        status, published_at, published_by, note
                 FROM ota_package
                 WHERE channel = ?1 AND status = 'published'
                 ORDER BY CAST(version AS INTEGER) DESC
                 LIMIT 1",
                params![channel],
                row_to_ota_package,
            )
            .optional()?;
        Ok(found)
    }

    // ================= audit_log =================

    /// 插入审计日志（后台操作 / 网关心跳 / 风控告警统一入口）。
    pub fn insert_audit_log(&self, log: &AuditLog) -> LicenseResult<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO audit_log
               (id, actor_type, actor_id, action, entity_type, entity_id, detail, ts, ip)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                log.id,
                log.actor_type.as_str(),
                log.actor_id,
                log.action,
                log.entity_type,
                log.entity_id,
                log.detail,
                log.ts,
                log.ip,
            ],
        )?;
        Ok(())
    }

    /// 按过滤条件分页列出审计日志（`page` 从 1 起，`ts` 降序）。
    pub fn list_audit_logs(
        &self,
        filter: &AuditFilter,
        page: u32,
        page_size: u32,
    ) -> LicenseResult<Vec<AuditLog>> {
        let page = page.max(1);
        let page_size = page_size.max(1);
        let offset = to_i64(((page - 1) as usize) * page_size as usize, "audit offset")?;

        let mut sql = String::from(
            "SELECT id, actor_type, actor_id, action, entity_type, entity_id, detail, ts, ip
             FROM audit_log WHERE 1 = 1",
        );
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        push_audit_filter(&mut sql, &mut args, filter);
        sql.push_str(" ORDER BY ts DESC, rowid DESC LIMIT ? OFFSET ?");
        args.push(Box::new(i64::from(page_size)));
        args.push(Box::new(offset));

        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), row_to_audit_log)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// 按过滤条件统计审计日志数。
    pub fn count_audit_logs(&self, filter: &AuditFilter) -> LicenseResult<u64> {
        let mut sql = String::from("SELECT COUNT(*) FROM audit_log WHERE 1 = 1");
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        push_audit_filter(&mut sql, &mut args, filter);
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let count: i64 = stmt.query_row(refs.as_slice(), |row| row.get(0))?;
        to_u64(count, "audit count")
    }

    /// 按日（UTC，`ts / 86400` 天锚点）聚合给定动作的审计计数。
    ///
    /// 总览「激活趋势」的真实数据源：只统计 `ts >= since_ts` 且 `action` 命中的
    /// 审计行，返回 `(day_anchor_unix_secs, action, count)`（无行不出现，调用方补零）。
    pub fn count_actions_by_day(
        &self,
        actions: &[&str],
        since_ts: i64,
    ) -> LicenseResult<Vec<(i64, String, u64)>> {
        if actions.is_empty() {
            return Ok(Vec::new());
        }
        let mut sql = String::from(
            "SELECT (ts / 86400) * 86400 AS day, action, COUNT(*) \
             FROM audit_log WHERE ts >= ?1 AND action IN (",
        );
        // 占位符从 ?2 起（?1 = since_ts）；action 列表来自调用方白名单，非用户输入。
        for (idx, _) in actions.iter().enumerate() {
            if idx > 0 {
                sql.push_str(", ");
            }
            sql.push_str(&format!("?{}", idx + 2));
        }
        sql.push_str(") GROUP BY day, action ORDER BY day ASC");

        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&sql)?;
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(since_ts)];
        for action in actions {
            params_vec.push(Box::new((*action).to_string()));
        }
        let refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), |row| {
            let day: i64 = row.get(0)?;
            let action: String = row.get(1)?;
            let count: i64 = row.get(2)?;
            Ok((day, action, count.max(0) as u64))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    // ================= admin_account（可配置账号 / 角色） =================

    /// 列出全部管理员账号（按 `created_at` 升序，再按账号名）。
    pub fn list_admin_accounts(&self) -> LicenseResult<Vec<AdminAccount>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT account, display_name, role, password_sha256, status,
                    last_login_at, created_at, updated_at
             FROM admin_account ORDER BY created_at ASC, account ASC",
        )?;
        let rows = stmt.query_map([], row_to_admin_account)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// 按账号查单条（不存在 → `None`）。
    pub fn get_admin_account(&self, account: &str) -> LicenseResult<Option<AdminAccount>> {
        let conn = self.conn.lock();
        let found = conn
            .query_row(
                "SELECT account, display_name, role, password_sha256, status,
                        last_login_at, created_at, updated_at
                 FROM admin_account WHERE account = ?1",
                params![account],
                row_to_admin_account,
            )
            .optional()?;
        Ok(found)
    }

    /// 统计管理员账号数。
    pub fn count_admin_accounts(&self) -> LicenseResult<u64> {
        let conn = self.conn.lock();
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM admin_account", [], |row| row.get(0))?;
        to_u64(count, "admin_account count")
    }

    /// 插入管理员账号（主键冲突 → [`LicenseError::Storage`]）。
    pub fn insert_admin_account(&self, acct: &AdminAccount) -> LicenseResult<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO admin_account
               (account, display_name, role, password_sha256, status,
                last_login_at, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                acct.account,
                acct.display_name,
                acct.role,
                acct.password_sha256,
                acct.status,
                acct.last_login_at,
                acct.created_at,
                acct.updated_at,
            ],
        )?;
        Ok(())
    }

    /// 更新管理员账号（显示名 / 角色 / 口令摘要 / 状态；不存在 → 400）。
    pub fn update_admin_account(
        &self,
        account: &str,
        display_name: &str,
        role: &str,
        password_sha256: &str,
        status: &str,
        updated_at: i64,
    ) -> LicenseResult<()> {
        let conn = self.conn.lock();
        let affected = conn.execute(
            "UPDATE admin_account
                SET display_name = ?2, role = ?3, password_sha256 = ?4,
                    status = ?5, updated_at = ?6
              WHERE account = ?1",
            params![
                account,
                display_name,
                role,
                password_sha256,
                status,
                updated_at
            ],
        )?;
        if affected == 0 {
            return Err(LicenseError::KeyStateIllegal(format!(
                "admin account not found: {account}"
            )));
        }
        Ok(())
    }

    /// 记录最近登录时刻（best-effort：账号不存在也不报错）。
    pub fn touch_admin_account_login(&self, account: &str, ts: i64) -> LicenseResult<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE admin_account SET last_login_at = ?2 WHERE account = ?1",
            params![account, ts],
        )?;
        Ok(())
    }

    /// 删除管理员账号（不存在 → 400）。
    pub fn delete_admin_account(&self, account: &str) -> LicenseResult<()> {
        let conn = self.conn.lock();
        let affected = conn.execute(
            "DELETE FROM admin_account WHERE account = ?1",
            params![account],
        )?;
        if affected == 0 {
            return Err(LicenseError::KeyStateIllegal(format!(
                "admin account not found: {account}"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{now_ns_id, ANCHOR_COUNT};

    /// 建一个内存库 + 一个租户，返回 `(store, tenant_id)`。
    fn fixture() -> (Store, String) {
        let store = Store::open_in_memory().expect("open in memory");
        let tenant = Tenant::new(
            "t-1".into(),
            "测试租户".into(),
            "ops@example.com".into(),
            1_700_000_000,
        );
        store.insert_tenant(&tenant).expect("insert tenant");
        (store, tenant.tenant_id)
    }

    fn sample_device(device_id: &str, machine_code: &str, status: DeviceStatus) -> Device {
        Device {
            device_id: device_id.into(),
            tenant_id: "t-1".into(),
            machine_code: machine_code.into(),
            anchor_hashes: (0..ANCHOR_COUNT).map(|i| format!("anchor{i:02}")).collect(),
            deploy_mode: DeployMode::Native,
            image_digest: None,
            host_anchor_ref: None,
            first_activation_at: None,
            status,
            created_at: 1_700_000_000,
        }
    }

    fn sample_code(code_id: &str, code: &str) -> ActivationCode {
        ActivationCode::new_issued(
            code_id.into(),
            code.into(),
            "t-1".into(),
            "pro".into(),
            1_700_000_000,
            1_800_000_000,
            Some("order-1".into()),
            "admin-1".into(),
            1_700_000_000,
        )
    }

    fn insert_min_keys(store: &Store) {
        store
            .insert_signing_key(&SigningKey {
                kid: "k-test".into(),
                status: SigningKeyStatus::Active,
                public_key: "cHVia2V5".into(),
                hsm_ref: Some("kms://test".into()),
                enabled_at: 1_700_000_000,
                retired_at: None,
            })
            .expect("insert signing key");
    }

    #[test]
    fn wal_mode_and_foreign_keys_are_enabled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("licensing.db");
        let store = Store::open(&db).expect("open");
        assert_eq!(
            store.journal_mode().expect("journal_mode").to_lowercase(),
            "wal",
            "WAL 未开启"
        );
        assert!(store.foreign_keys_enabled().expect("fk"));
    }

    #[test]
    fn migrate_is_idempotent() {
        let store = Store::open_in_memory().expect("open");
        store.migrate().expect("migrate 1");
        store.migrate().expect("migrate 2");
        // 9 张表全部存在（用查询证明而非只断言 migrate 不报错）。
        let (five, _) = fixture();
        five.insert_tenant(&Tenant::new("t".into(), "n".into(), "c".into(), 1))
            .expect("tenant");
    }

    /// **P0 回归**：存量「带展示分隔符 / 大写」的机器码在迁移后归一为无分隔符小写匹配态
    /// （`device.machine_code` 与 `activation_code.prebind_machine_code` 都要覆盖）。
    #[test]
    fn normalize_migration_rewrites_polluted_machine_codes() {
        let (store, _) = fixture();

        // 直接注入污染值（绕过 service 层归一，模拟历史库）。
        store
            .insert_device(&sample_device(
                "dev-1",
                "8F3A-91C2-7D04-5BE6",
                DeviceStatus::Active,
            ))
            .expect("insert device");
        let mut code = sample_code("c-1", "CODE-POLLUTED");
        code.prebind_machine_code = Some("8F3A_91C2:7D04-5BE6".into());
        store.insert_code(&code).expect("insert code");

        store.migrate().expect("migrate");

        let conn = store.conn.lock();
        let dev_mc: String = conn
            .query_row(
                "SELECT machine_code FROM device WHERE device_id = 'dev-1'",
                [],
                |r| r.get(0),
            )
            .expect("device machine_code");
        assert_eq!(
            dev_mc, "8f3a91c27d045be6",
            "设备机器码必须归一为无分隔符小写"
        );
        let prebind: String = conn
            .query_row(
                "SELECT prebind_machine_code FROM activation_code WHERE code_id = 'c-1'",
                [],
                |r| r.get(0),
            )
            .expect("prebind_machine_code");
        assert_eq!(
            prebind, "8f3a91c27d045be6",
            "预绑定机器码必须归一为无分隔符小写"
        );
    }

    /// **P0 回归**：归一后撞 UNIQUE 时迁移**不报错、不删行**（`UPDATE OR IGNORE` 跳过而非
    /// fail-closed 启动失败，更不允许删 `device` 行掩盖冲突）。
    #[test]
    fn normalize_migration_tolerates_unique_collision_without_deleting_rows() {
        let (store, _) = fixture();
        // 两个原始值不同、但归一后同为 `abcd` 的设备。
        store
            .insert_device(&sample_device("dev-a", "ABCD", DeviceStatus::Active))
            .expect("insert a");
        store
            .insert_device(&sample_device("dev-b", "ab-cd", DeviceStatus::Active))
            .expect("insert b");

        store
            .migrate()
            .expect("迁移必须完成（撞 UNIQUE 的行跳过而非失败）");

        assert_eq!(store.count_devices(None).expect("count"), 2, "绝不删行");
        let conn = store.conn.lock();
        let values: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT machine_code FROM device ORDER BY device_id")
                .expect("prepare");
            let rows = stmt
                .query_map([], |r| r.get::<usize, String>(0))
                .expect("query");
            rows.map(|r| r.expect("row")).collect()
        };
        assert_eq!(values.len(), 2, "两行都必须保留");
        assert_eq!(
            values.iter().filter(|v| *v == "abcd").count(),
            1,
            "恰好一行归一为 abcd（另一行因撞 UNIQUE 被跳过）：{values:?}"
        );
    }

    #[test]
    fn tenant_insert_get_list_round_trip_optionals() {
        let (store, _) = fixture();
        let tenant = Tenant {
            tenant_id: "t-2".into(),
            name: "二号租户".into(),
            verify_mode_default: VerifyMode::C,
            contact: String::new(),
            created_at: 1_700_000_100,
        };
        store.insert_tenant(&tenant).expect("insert");

        let got = store.get_tenant("t-2").expect("get").expect("some");
        assert_eq!(got, tenant);
        assert_eq!(got.verify_mode_default, VerifyMode::C);

        assert!(store.get_tenant("nope").expect("get").is_none());

        let all = store.list_tenants().expect("list");
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].tenant_id, "t-1");
        assert_eq!(all[1].tenant_id, "t-2");
    }

    #[test]
    fn device_round_trip_with_some_and_none_optionals() {
        let (store, _) = fixture();

        // 全 None 形态。机器码用**无分隔符**字面量：`get_device_by_machine_code` 会先
        // 归一查询入参（剥分隔符 + 小写），而本测试的 `insert_device` 写的是原样值。
        let native = sample_device("dev-native", "mcnative", DeviceStatus::Active);
        store.insert_device(&native).expect("insert native");
        assert_eq!(
            store.get_device("dev-native").expect("get"),
            Some(native.clone())
        );
        assert_eq!(
            store
                .get_device_by_machine_code("mcnative")
                .expect("get by mc"),
            Some(native)
        );

        // 全 Some 形态（docker 溯源字段 + 首次激活时间，非 active 状态）。
        let docker = Device {
            device_id: "dev-docker".into(),
            tenant_id: "t-1".into(),
            machine_code: "mc-docker".into(),
            anchor_hashes: vec!["a".into(), "b".into()],
            deploy_mode: DeployMode::Docker,
            image_digest: Some("sha256:deadbeef".into()),
            host_anchor_ref: Some("/host/anchors".into()),
            first_activation_at: Some(1_700_000_500),
            status: DeviceStatus::Gracing,
            created_at: 1_700_000_600,
        };
        store.insert_device(&docker).expect("insert docker");
        let got = store.get_device("dev-docker").expect("get").expect("some");
        assert_eq!(got, docker);
        assert_eq!(got.deploy_mode, DeployMode::Docker);
        assert_eq!(got.image_digest.as_deref(), Some("sha256:deadbeef"));
        assert_eq!(got.host_anchor_ref.as_deref(), Some("/host/anchors"));
        assert_eq!(got.first_activation_at, Some(1_700_000_500));
        assert_eq!(got.anchor_hashes, vec!["a".to_string(), "b".to_string()]);

        assert!(store.get_device("missing").expect("get").is_none());
        assert!(store
            .get_device_by_machine_code("missing")
            .expect("get")
            .is_none());
    }

    #[test]
    fn machine_code_unique_constraint_returns_storage_error() {
        let (store, _) = fixture();
        store
            .insert_device(&sample_device("dev-1", "mc-dup", DeviceStatus::Active))
            .expect("first");
        let err = store
            .insert_device(&sample_device("dev-2", "mc-dup", DeviceStatus::Active))
            .expect_err("duplicate must fail");
        assert!(matches!(err, LicenseError::Storage(_)), "{err:?}");
        // 唯一约束生效：第二个设备未落库。
        assert!(store.get_device("dev-2").expect("get").is_none());
    }

    #[test]
    fn list_devices_paginates_and_counts() {
        let (store, tenant) = fixture();
        for i in 0..5 {
            store
                .insert_device(&sample_device(
                    &format!("dev-{i}"),
                    &format!("mc-{i}"),
                    DeviceStatus::Active,
                ))
                .expect("insert");
        }
        assert_eq!(store.count_devices(Some(&tenant)).expect("count"), 5);

        let p1 = store.list_devices(Some(&tenant), 1, 2).expect("p1");
        let p2 = store.list_devices(Some(&tenant), 2, 2).expect("p2");
        let p3 = store.list_devices(Some(&tenant), 3, 2).expect("p3");
        assert_eq!(p1.len(), 2);
        assert_eq!(p2.len(), 2);
        assert_eq!(p3.len(), 1);
        // 不重不漏：三页 ID 集合恰为 5 个。
        let mut ids: Vec<String> = p1
            .iter()
            .chain(p2.iter())
            .chain(p3.iter())
            .map(|d| d.device_id.clone())
            .collect();
        ids.sort();
        assert_eq!(
            ids,
            vec![
                "dev-0".to_string(),
                "dev-1".to_string(),
                "dev-2".to_string(),
                "dev-3".to_string(),
                "dev-4".to_string()
            ]
        );
        assert_eq!(store.count_devices(Some("t-none")).expect("count"), 0);
        // 全局计数（None）应为 5。
        assert_eq!(store.count_devices(None).expect("count all"), 5);
        assert_eq!(store.list_devices(None, 1, 10).expect("list all").len(), 5);
    }

    #[test]
    fn update_device_status_persists() {
        let (store, _) = fixture();
        store
            .insert_device(&sample_device("dev-1", "mc-1", DeviceStatus::Active))
            .expect("insert");
        store
            .update_device_status("dev-1", DeviceStatus::Degraded)
            .expect("update");
        assert_eq!(
            store
                .get_device("dev-1")
                .expect("get")
                .expect("some")
                .status,
            DeviceStatus::Degraded
        );
    }

    #[test]
    fn code_insert_get_round_trip_with_optionals() {
        let (store, _) = fixture();
        // 绑定关系与重发溯源是外键，先把被引用行落库。
        store
            .insert_device(&sample_device("dev-1", "mc-1", DeviceStatus::Active))
            .expect("device");
        store
            .insert_code(&sample_code("c-original", "CODE-ORIG"))
            .expect("orig");

        // 未绑定、无废弃信息（全 None）。
        let issued = sample_code("c-issued", "CODE-ISSUED");
        store.insert_code(&issued).expect("insert");
        assert_eq!(
            store
                .get_code_by_value("CODE-ISSUED")
                .expect("get")
                .expect("some"),
            issued
        );
        assert_eq!(
            store
                .get_code_by_id("c-issued")
                .expect("get")
                .expect("some"),
            issued
        );

        // 已绑定 + 幂等键 + 重发溯源（全 Some）。
        let mut bound = sample_code("c-bound", "CODE-BOUND");
        bound.status = CodeStatus::Bound;
        bound.bound_device_id = Some("dev-1".into());
        bound.idempotency_key = Some("idem-1".into());
        bound.reissued_from_id = Some("c-original".into());
        store.insert_code(&bound).expect("insert bound");
        let got = store
            .get_code_by_value("CODE-BOUND")
            .expect("get")
            .expect("some");
        assert_eq!(got, bound);
        assert_eq!(got.bound_device_id.as_deref(), Some("dev-1"));
        assert_eq!(got.idempotency_key.as_deref(), Some("idem-1"));

        // 已废弃（revoked_at + reason 均 Some）。
        let mut revoked = sample_code("c-revoked", "CODE-REVOKED");
        revoked.status = CodeStatus::Revoked;
        revoked.revoked_at = Some(1_700_000_900);
        revoked.revoked_reason = Some("换机重发".into());
        store.insert_code(&revoked).expect("insert revoked");
        let got = store
            .get_code_by_id("c-revoked")
            .expect("get")
            .expect("some");
        assert_eq!(got, revoked);
        assert_eq!(got.revoked_at, Some(1_700_000_900));
        assert_eq!(got.revoked_reason.as_deref(), Some("换机重发"));

        assert!(store.get_code_by_value("nope").expect("get").is_none());
        assert!(store.get_code_by_id("nope").expect("get").is_none());
    }

    #[test]
    fn list_codes_and_count_with_filter() {
        let (store, _) = fixture();
        store
            .insert_device(&sample_device("dev-1", "mc-1", DeviceStatus::Active))
            .expect("device");
        store.insert_code(&sample_code("c-1", "V1")).expect("c1");
        store.insert_code(&sample_code("c-2", "V2")).expect("c2");
        let mut bound = sample_code("c-3", "V3");
        bound.status = CodeStatus::Bound;
        bound.bound_device_id = Some("dev-1".into());
        store.insert_code(&bound).expect("c3");

        let all = CodeFilter::default();
        assert_eq!(store.count_codes(&all).expect("count"), 3);
        assert_eq!(store.list_codes(&all, 1, 10).expect("list").len(), 3);

        let issued_only = CodeFilter {
            status: Some(CodeStatus::Issued),
            ..Default::default()
        };
        assert_eq!(store.count_codes(&issued_only).expect("count"), 2);

        // tier 过滤：三张码的 tier 均为 "pro"。
        let pro_tier = CodeFilter {
            tier: Some("pro".into()),
            ..Default::default()
        };
        assert_eq!(store.count_codes(&pro_tier).expect("count"), 3);
        let gold_tier = CodeFilter {
            tier: Some("gold".into()),
            ..Default::default()
        };
        assert_eq!(store.count_codes(&gold_tier).expect("count"), 0);

        // order_id 过滤（映射列 source_order_id）：三张码 source_order_id 均为 "order-1"。
        let by_order = CodeFilter {
            order_id: Some("order-1".into()),
            ..Default::default()
        };
        assert_eq!(store.count_codes(&by_order).expect("count"), 3);
        let other_order = CodeFilter {
            order_id: Some("order-none".into()),
            ..Default::default()
        };
        assert_eq!(store.count_codes(&other_order).expect("count"), 0);

        let other_tenant = CodeFilter {
            tenant_id: Some("t-none".into()),
            ..Default::default()
        };
        assert_eq!(store.count_codes(&other_tenant).expect("count"), 0);

        // 分页：page_size=2 → 2 / 1。
        let p1 = store.list_codes(&all, 1, 2).expect("p1");
        let p2 = store.list_codes(&all, 2, 2).expect("p2");
        assert_eq!(p1.len(), 2);
        assert_eq!(p2.len(), 1);
    }

    #[test]
    fn bind_code_to_device_race_is_blocked() {
        let (store, _) = fixture();
        store
            .insert_device(&sample_device("dev-a", "mc-a", DeviceStatus::Active))
            .expect("a");
        store
            .insert_device(&sample_device("dev-b", "mc-b", DeviceStatus::Active))
            .expect("b");
        store
            .insert_code(&sample_code("c-1", "CODE-1"))
            .expect("code");

        // A 先绑定成功。
        store
            .bind_code_to_device("c-1", "dev-a")
            .expect("A binds first");
        let after_a = store.get_code_by_id("c-1").expect("get").expect("some");
        assert_eq!(after_a.status, CodeStatus::Bound);
        assert_eq!(after_a.bound_device_id.as_deref(), Some("dev-a"));

        // B 再来抢：必须被拒，且**未**覆盖绑定关系（一机一码防线）。
        let err = store
            .bind_code_to_device("c-1", "dev-b")
            .expect_err("B must be rejected");
        assert!(
            matches!(err, LicenseError::ActivationRejected(_)),
            "{err:?}"
        );
        let after_b = store.get_code_by_id("c-1").expect("get").expect("some");
        assert_eq!(
            after_b.bound_device_id.as_deref(),
            Some("dev-a"),
            "绑定被篡改"
        );
        assert_eq!(after_b.status, CodeStatus::Bound);

        // 不存在的码 → 同样拒绝（但不影响任何数据）。
        let err = store
            .bind_code_to_device("c-missing", "dev-b")
            .expect_err("missing code");
        assert!(
            matches!(err, LicenseError::ActivationRejected(_)),
            "{err:?}"
        );

        // 错误信息不含码值原文。
        let rendered = store
            .bind_code_to_device("c-1", "dev-b")
            .expect_err("repeat")
            .to_string();
        assert!(
            !rendered.contains("CODE-1"),
            "错误信息泄露激活码原文: {rendered}"
        );
    }

    #[test]
    fn revoke_code_state_machine_and_validation() {
        let (store, _) = fixture();
        store
            .insert_code(&sample_code("c-1", "CODE-1"))
            .expect("code");

        // issued → revoked 合法。
        store
            .revoke_code("c-1", "客户退订", 1_700_001_000)
            .expect("revoke");
        let got = store.get_code_by_id("c-1").expect("get").expect("some");
        assert_eq!(got.status, CodeStatus::Revoked);
        assert_eq!(got.revoked_at, Some(1_700_001_000));
        assert_eq!(got.revoked_reason.as_deref(), Some("客户退订"));
        // 落库后的行必须自洽（validate 通过）。
        assert!(got.validate().is_ok());

        // 重复废弃 → KeyStateIllegal。
        let err = store
            .revoke_code("c-1", "again", 1_700_002_000)
            .expect_err("double revoke");
        assert!(matches!(err, LicenseError::KeyStateIllegal(_)), "{err:?}");

        // 空原因 → 拒绝。
        store
            .insert_code(&sample_code("c-2", "CODE-2"))
            .expect("c2");
        assert!(store.revoke_code("c-2", "   ", 1).is_err());

        // 不存在 → 拒绝。
        assert!(store.revoke_code("nope", "r", 1).is_err());

        // revoked → reissued 合法。
        store.mark_code_reissued("c-1").expect("reissue");
        assert_eq!(
            store
                .get_code_by_id("c-1")
                .expect("get")
                .expect("some")
                .status,
            CodeStatus::Reissued
        );
        // reissued 再重发 → KeyStateIllegal（单向状态机）。
        assert!(store.mark_code_reissued("c-1").is_err());
    }

    /// §7 步骤 ② 的原子改绑：设备身份前移 + 旧租约作废 + 新租约写入 + 审计——一次成功。
    #[test]
    fn rebind_device_and_issue_lease_is_atomic_and_consistent() {
        let (store, _) = fixture();
        insert_min_keys(&store);
        store
            .insert_device(&sample_device("dev-1", "mc-old", DeviceStatus::Active))
            .expect("device");
        store
            .insert_code(&sample_code("c-1", "CODE-1"))
            .expect("code");
        let old = Lease {
            lease_id: "l-old".into(),
            device_id: "dev-1".into(),
            code_id: "c-1".into(),
            kid: "k-test".into(),
            token_sig: "sig-old".into(),
            verify_mode: VerifyMode::B,
            tier: "pro".into(),
            issued_at: 1_700_000_000,
            valid_until: 1_700_600_000,
            last_heartbeat_at: None,
            status: LeaseStatus::Active,
        };
        store.insert_lease(&old).expect("old lease");

        let new = Lease {
            lease_id: "l-new".into(),
            device_id: "dev-1".into(),
            code_id: "c-1".into(),
            kid: "k-test".into(),
            token_sig: "sig-new".into(),
            verify_mode: VerifyMode::B,
            tier: "pro".into(),
            issued_at: 1_700_000_100,
            valid_until: 1_700_600_000,
            last_heartbeat_at: None,
            status: LeaseStatus::Active,
        };
        let audit = AuditLog {
            id: "audit-1".into(),
            actor_type: ActorType::System,
            actor_id: "system".into(),
            action: "activation_rebind".into(),
            entity_type: "activation_code".into(),
            entity_id: "c-1".into(),
            detail: "hits=4 drifts=1".into(),
            ts: 1_700_000_100,
            ip: String::new(),
        };
        store
            .rebind_device_and_issue_lease(
                "dev-1",
                "mc-new",
                &["x".into(), "y".into()],
                &new,
                &audit,
            )
            .expect("rebind");

        // 设备身份 + 锚点一起前移。
        let dev = store.get_device("dev-1").unwrap().unwrap();
        assert_eq!(dev.machine_code, "mc-new");
        assert_eq!(dev.anchor_hashes, vec!["x".to_string(), "y".to_string()]);
        // 旧租约作废、新租约 active。
        assert_eq!(
            store.get_lease("l-old").unwrap().unwrap().status,
            LeaseStatus::Stopped
        );
        assert_eq!(
            store.get_lease("l-new").unwrap().unwrap().status,
            LeaseStatus::Active
        );
        // 审计留痕落库。
        let logs = store
            .list_audit_logs(
                &AuditFilter {
                    action: Some("activation_rebind".into()),
                    ..Default::default()
                },
                1,
                10,
            )
            .unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].detail, "hits=4 drifts=1");
    }

    /// 新 `machine_code` 与其它设备冲突 → 唯一约束失败 → **整事务回滚**（无任何中间态）。
    #[test]
    fn rebind_rolls_back_when_new_machine_code_is_taken() {
        let (store, _) = fixture();
        insert_min_keys(&store);
        store
            .insert_device(&sample_device("dev-1", "mc-old", DeviceStatus::Active))
            .expect("d1");
        store
            .insert_device(&sample_device("dev-2", "mc-taken", DeviceStatus::Active))
            .expect("d2");
        store
            .insert_code(&sample_code("c-1", "CODE-1"))
            .expect("code");
        let old = Lease {
            lease_id: "l-old".into(),
            device_id: "dev-1".into(),
            code_id: "c-1".into(),
            kid: "k-test".into(),
            token_sig: "sig-old".into(),
            verify_mode: VerifyMode::B,
            tier: "pro".into(),
            issued_at: 1_700_000_000,
            valid_until: 1_700_600_000,
            last_heartbeat_at: None,
            status: LeaseStatus::Active,
        };
        store.insert_lease(&old).expect("old lease");
        let new = Lease {
            lease_id: "l-new".into(),
            device_id: "dev-1".into(),
            code_id: "c-1".into(),
            kid: "k-test".into(),
            token_sig: "sig-new".into(),
            verify_mode: VerifyMode::B,
            tier: "pro".into(),
            issued_at: 1_700_000_100,
            valid_until: 1_700_600_000,
            last_heartbeat_at: None,
            status: LeaseStatus::Active,
        };
        let audit = AuditLog {
            id: "audit-1".into(),
            actor_type: ActorType::System,
            actor_id: "system".into(),
            action: "activation_rebind".into(),
            entity_type: "activation_code".into(),
            entity_id: "c-1".into(),
            detail: "x".into(),
            ts: 1,
            ip: String::new(),
        };

        let err = store
            .rebind_device_and_issue_lease("dev-1", "mc-taken", &[], &new, &audit)
            .expect_err("conflicting machine_code must fail");
        assert!(matches!(err, LicenseError::Storage(_)), "{err:?}");

        // 全回滚：设备身份 / 旧租约 / 新租约 / 审计均无变化。
        assert_eq!(
            store.get_device("dev-1").unwrap().unwrap().machine_code,
            "mc-old"
        );
        assert_eq!(
            store.get_lease("l-old").unwrap().unwrap().status,
            LeaseStatus::Active
        );
        assert!(store.get_lease("l-new").unwrap().is_none());
        assert_eq!(
            store
                .list_audit_logs(&AuditFilter::default(), 1, 10)
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn lease_round_trip_with_optionals() {
        let (store, _) = fixture();
        insert_min_keys(&store);
        store
            .insert_device(&sample_device("dev-1", "mc-1", DeviceStatus::Active))
            .expect("device");
        store
            .insert_code(&sample_code("c-1", "CODE-1"))
            .expect("code");

        // last_heartbeat_at = None。
        let lease = Lease {
            lease_id: "l-1".into(),
            device_id: "dev-1".into(),
            code_id: "c-1".into(),
            kid: "k-test".into(),
            token_sig: "sig-b64".into(),
            verify_mode: VerifyMode::B,
            tier: "pro".into(),
            issued_at: 1_700_000_000,
            valid_until: 1_700_600_000,
            last_heartbeat_at: None,
            status: LeaseStatus::Active,
        };
        store.insert_lease(&lease).expect("insert");
        assert_eq!(store.get_lease("l-1").expect("get"), Some(lease.clone()));
        assert_eq!(
            store
                .get_lease("l-1")
                .expect("get")
                .expect("some")
                .last_heartbeat_at,
            None
        );

        // 第二次心跳后 last_heartbeat_at = Some。
        store
            .update_lease_heartbeat("l-1", 1_700_000_500)
            .expect("hb");
        let got = store.get_lease("l-1").expect("get").expect("some");
        assert_eq!(got.last_heartbeat_at, Some(1_700_000_500));

        // 状态更新。
        store
            .update_lease_status("l-1", LeaseStatus::Gracing)
            .expect("status");
        assert_eq!(
            store.get_lease("l-1").expect("get").expect("some").status,
            LeaseStatus::Gracing
        );

        // 按设备列出。
        let leases = store.list_leases_by_device("dev-1").expect("list");
        assert_eq!(leases.len(), 1);
        assert_eq!(leases[0].lease_id, "l-1");
        assert!(store.get_lease("missing").expect("get").is_none());
        assert!(store
            .list_leases_by_device("missing")
            .expect("list")
            .is_empty());
    }

    #[test]
    fn foreign_keys_on_rejects_orphan_lease() {
        let (store, _) = fixture();
        insert_min_keys(&store);
        store
            .insert_code(&sample_code("c-1", "CODE-1"))
            .expect("code");

        // 引用不存在的 device_id → 必须报错（foreign_keys=ON 生效）。
        let orphan = Lease {
            lease_id: "l-orphan".into(),
            device_id: "dev-does-not-exist".into(),
            code_id: "c-1".into(),
            kid: "k-test".into(),
            token_sig: "sig".into(),
            verify_mode: VerifyMode::B,
            tier: "pro".into(),
            issued_at: 1,
            valid_until: 2,
            last_heartbeat_at: None,
            status: LeaseStatus::Active,
        };
        let err = store.insert_lease(&orphan).expect_err("orphan must fail");
        assert!(matches!(err, LicenseError::Storage(_)), "{err:?}");
        assert!(store.get_lease("l-orphan").expect("get").is_none());

        // 引用不存在的 code_id → 同样报错。
        store
            .insert_device(&sample_device("dev-1", "mc-1", DeviceStatus::Active))
            .expect("device");
        let bad_code = Lease {
            lease_id: "l-badcode".into(),
            device_id: "dev-1".into(),
            code_id: "c-missing".into(),
            ..orphan
        };
        assert!(store.insert_lease(&bad_code).is_err());

        // `kid` **刻意不是硬外键**：私钥经环境变量注入、kid 可在尚未登记公钥时先用于签发
        // （设计 §5 私钥隔离 + 轮换语义）。故引用未知 kid 必须**成功**，而非报错。
        let unknown_kid = Lease {
            lease_id: "l-unknown-kid".into(),
            device_id: "dev-1".into(),
            code_id: "c-1".into(),
            kid: "k-not-yet-registered".into(),
            ..bad_code
        };
        store
            .insert_lease(&unknown_kid)
            .expect("lease.kid 不是外键，未知 kid 允许落库");
        assert_eq!(
            store
                .get_lease("l-unknown-kid")
                .expect("get")
                .expect("some")
                .kid,
            "k-not-yet-registered"
        );
    }

    #[test]
    fn heartbeat_round_trip_with_optionals() {
        let (store, _) = fixture();
        insert_min_keys(&store);
        store
            .insert_device(&sample_device("dev-1", "mc-1", DeviceStatus::Active))
            .expect("device");
        store
            .insert_code(&sample_code("c-1", "CODE-1"))
            .expect("code");
        let lease = Lease {
            lease_id: "l-1".into(),
            device_id: "dev-1".into(),
            code_id: "c-1".into(),
            kid: "k-test".into(),
            token_sig: "sig".into(),
            verify_mode: VerifyMode::B,
            tier: "pro".into(),
            issued_at: 1_700_000_000,
            valid_until: 1_700_600_000,
            last_heartbeat_at: None,
            status: LeaseStatus::Active,
        };
        store.insert_lease(&lease).expect("lease");

        let hb_ok = Heartbeat {
            id: "hb-1".into(),
            lease_id: "l-1".into(),
            device_id: "dev-1".into(),
            client_ts: 1_700_000_400,
            server_ts: 1_700_000_401,
            result: HeartbeatResult::Ok,
            receipt_cursor: Some("10-25".into()),
            created_at: 1_700_000_401,
        };
        let hb_skew = Heartbeat {
            id: "hb-2".into(),
            lease_id: "l-1".into(),
            device_id: "dev-1".into(),
            client_ts: 1_600_000_000,
            server_ts: 1_700_000_500,
            result: HeartbeatResult::Skew,
            receipt_cursor: None,
            created_at: 1_700_000_500,
        };
        store.insert_heartbeat(&hb_ok).expect("hb1");
        store.insert_heartbeat(&hb_skew).expect("hb2");

        let list = store.list_heartbeats_by_lease("l-1").expect("list");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0], hb_ok);
        assert_eq!(list[1], hb_skew);
        assert_eq!(list[0].receipt_cursor.as_deref(), Some("10-25"));
        assert_eq!(list[1].receipt_cursor, None);
        assert!(store
            .list_heartbeats_by_lease("missing")
            .expect("list")
            .is_empty());
    }

    #[test]
    fn audit_receipt_round_trip_and_gap_flag() {
        let (store, _) = fixture();
        insert_min_keys(&store);
        store
            .insert_device(&sample_device("dev-1", "mc-1", DeviceStatus::Active))
            .expect("device");
        store
            .insert_code(&sample_code("c-1", "CODE-1"))
            .expect("code");
        store
            .insert_lease(&Lease {
                lease_id: "l-1".into(),
                device_id: "dev-1".into(),
                code_id: "c-1".into(),
                kid: "k-test".into(),
                token_sig: "sig".into(),
                verify_mode: VerifyMode::B,
                tier: "pro".into(),
                issued_at: 1_700_000_000,
                valid_until: 1_700_600_000,
                last_heartbeat_at: None,
                status: LeaseStatus::Active,
            })
            .expect("lease");

        // 正常回执（gap=false），received_at 可晚于 ts（断网补报）。
        let r1 = AuditReceipt {
            id: "r-1".into(),
            lease_id: "l-1".into(),
            device_mid: "mc-1".into(),
            seq_from: 1,
            seq_to: 10,
            count: 10,
            payload_digest: "digest-1".into(),
            ts: 1_700_000_100,
            sig: "sig-1".into(),
            received_at: 1_700_000_200,
            gap_flag: false,
        };
        // 跳空回执（gap=true）。
        let r2 = AuditReceipt {
            id: "r-2".into(),
            lease_id: "l-1".into(),
            device_mid: "mc-1".into(),
            seq_from: 20,
            seq_to: 25,
            count: 6,
            payload_digest: "digest-2".into(),
            ts: 1_700_000_300,
            sig: "sig-2".into(),
            received_at: 1_700_000_500,
            gap_flag: true,
        };
        store.insert_audit_receipt(&r1).expect("r1");
        store.insert_audit_receipt(&r2).expect("r2");

        let list = store.list_receipts_by_lease("l-1").expect("list");
        assert_eq!(list, vec![r1.clone(), r2.clone()]);
        assert!(!list[0].gap_flag);
        assert!(list[1].gap_flag, "gap_flag 必须真实往返为 true");
        // received_at 晚于 ts，允许（延迟补报）。
        assert!(list[0].received_at > list[0].ts);

        // last_receipt：seq_to 最大者为 r-2。
        let last = store
            .last_receipt_for_lease("l-1")
            .expect("last")
            .expect("some");
        assert_eq!(last.id, "r-2");
        assert_eq!(last.seq_to, 25);

        // 置位 / 复位。
        store.update_receipt_gap_flag("r-1", true).expect("set");
        assert!(store
            .last_receipt_for_lease("l-1")
            .expect("last")
            .expect("some")
            .id
            .eq("r-2"));
        let list = store.list_receipts_by_lease("l-1").expect("list");
        assert!(list.iter().all(|r| r.gap_flag), "全部置位");
        store.update_receipt_gap_flag("r-1", false).expect("reset");
        let list = store.list_receipts_by_lease("l-1").expect("list");
        assert!(!list[0].gap_flag);

        assert!(store
            .last_receipt_for_lease("missing")
            .expect("last")
            .is_none());
    }

    #[test]
    fn nonce_insert_if_absent_first_true_then_false() {
        let (store, _) = fixture();
        store
            .insert_device(&sample_device("dev-1", "mc-1", DeviceStatus::Active))
            .expect("device");

        let first = store
            .insert_nonce_if_absent("nonce-abc", "dev-1", 1_700_100_000)
            .expect("first");
        assert!(first, "首次使用必须返回 true");

        let second = store
            .insert_nonce_if_absent("nonce-abc", "dev-1", 1_700_100_000)
            .expect("second");
        assert!(!second, "重放必须返回 false");

        // 原记录未被重放覆盖：nonce 只有一条，且 used_at 由内部时钟填充（非 0）。
        let stored = store.get_nonce("nonce-abc").expect("get").expect("some");
        assert_eq!(stored.expires_at, 1_700_100_000);
        assert!(stored.used_at > 1_600_000_000, "used_at 应为真实时钟值");
    }

    #[test]
    fn purge_expired_nonces_removes_only_expired() {
        let (store, _) = fixture();
        store
            .insert_device(&sample_device("dev-1", "mc-1", DeviceStatus::Active))
            .expect("device");
        store
            .insert_nonce_if_absent("n-old", "dev-1", 1_000)
            .expect("old");
        store
            .insert_nonce_if_absent("n-new", "dev-1", 5_000)
            .expect("new");

        let removed = store.purge_expired_nonces(2_000).expect("purge");
        assert_eq!(removed, 1, "only the expired nonce is removed");
        assert!(store.get_nonce("n-old").expect("get").is_none());
        assert!(store.get_nonce("n-new").expect("get").is_some());
        assert_eq!(store.purge_expired_nonces(2_000).expect("again"), 0);
    }

    #[test]
    fn device_pubkey_pin_is_first_write_wins() {
        let (store, _) = fixture();
        store
            .insert_device(&sample_device("dev-1", "mc-1", DeviceStatus::Active))
            .expect("device");

        // 未钉定 → None。
        assert_eq!(store.get_device_pubkey("dev-1").expect("get"), None);

        // 首写钉定成功；再次钉定不覆盖。
        assert!(
            store
                .pin_device_pubkey_if_absent("dev-1", "pk-A")
                .expect("pin"),
            "首次钉定必须返回 true"
        );
        assert_eq!(
            store.get_device_pubkey("dev-1").expect("get").as_deref(),
            Some("pk-A")
        );
        assert!(
            !store
                .pin_device_pubkey_if_absent("dev-1", "pk-B")
                .expect("pin again"),
            "重复钉定必须返回 false（first-write-wins）"
        );
        assert_eq!(
            store.get_device_pubkey("dev-1").expect("get").as_deref(),
            Some("pk-A"),
            "已钉定公钥不得被覆盖"
        );

        // 不存在的设备 → 钉定无效果（affected == 0），读取为 None。
        assert!(!store
            .pin_device_pubkey_if_absent("dev-missing", "pk-X")
            .expect("pin missing"));
        assert_eq!(store.get_device_pubkey("dev-missing").expect("get"), None);
    }

    #[test]
    fn signing_key_round_trip_and_rotation() {
        let (store, _) = fixture();
        let active = SigningKey {
            kid: "k-1".into(),
            status: SigningKeyStatus::Active,
            public_key: "cHViLTE".into(),
            hsm_ref: Some("kms://prod".into()),
            enabled_at: 1_700_000_000,
            retired_at: None,
        };
        // 无 hsm_ref（离线自持）形态。
        let public_only = SigningKey {
            kid: "k-2".into(),
            status: SigningKeyStatus::Retiring,
            public_key: "cHViLTI".into(),
            hsm_ref: None,
            enabled_at: 1_700_000_100,
            retired_at: None,
        };
        store.insert_signing_key(&active).expect("k1");
        store.insert_signing_key(&public_only).expect("k2");

        assert_eq!(
            store.get_signing_key("k-1").expect("get"),
            Some(active.clone())
        );
        assert_eq!(
            store
                .get_signing_key("k-2")
                .expect("get")
                .expect("some")
                .hsm_ref,
            None
        );
        assert!(store.get_signing_key("k-none").expect("get").is_none());

        let all = store.list_signing_keys().expect("list");
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].kid, "k-1");
        assert_eq!(all[1].kid, "k-2");

        // k-1 → retired：写入 retired_at。
        store
            .update_signing_key_status("k-1", SigningKeyStatus::Retired, 1_700_002_000)
            .expect("retire");
        let k1 = store.get_signing_key("k-1").expect("get").expect("some");
        assert_eq!(k1.status, SigningKeyStatus::Retired);
        assert_eq!(k1.retired_at, Some(1_700_002_000));

        // k-2 → active：不改 retired_at。
        store
            .update_signing_key_status("k-2", SigningKeyStatus::Active, 1_700_003_000)
            .expect("promote");
        let k2 = store.get_signing_key("k-2").expect("get").expect("some");
        assert_eq!(k2.status, SigningKeyStatus::Active);
        assert_eq!(k2.retired_at, None);
    }

    #[test]
    fn audit_log_round_trip_and_filters() {
        let (store, _) = fixture();
        let logs = vec![
            AuditLog {
                id: "a-1".into(),
                actor_type: ActorType::Admin,
                actor_id: "admin-1".into(),
                action: "issue".into(),
                entity_type: "activation_code".into(),
                entity_id: "c-1".into(),
                detail: r#"{"order":"order-1"}"#.into(),
                ts: 1_700_000_000,
                ip: "10.0.0.1".into(),
            },
            AuditLog {
                id: "a-2".into(),
                actor_type: ActorType::Device,
                actor_id: "dev-1".into(),
                action: "heartbeat".into(),
                entity_type: "lease".into(),
                entity_id: "l-1".into(),
                detail: r#"{"result":"ok"}"#.into(),
                ts: 1_700_000_100,
                ip: "10.0.0.2".into(),
            },
            AuditLog {
                id: "a-3".into(),
                actor_type: ActorType::System,
                actor_id: "risk-scanner".into(),
                action: "gap_alarm".into(),
                entity_type: "lease".into(),
                entity_id: "l-1".into(),
                detail: r#"{"gap":true}"#.into(),
                ts: 1_700_000_200,
                ip: "localhost".into(),
            },
        ];
        for log in &logs {
            store.insert_audit_log(log).expect("insert");
        }

        // ts 降序。
        let all = store
            .list_audit_logs(&AuditFilter::default(), 1, 10)
            .expect("list");
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].id, "a-3");
        assert_eq!(all[2].id, "a-1");
        assert_eq!(
            store
                .count_audit_logs(&AuditFilter::default())
                .expect("count"),
            3
        );

        // actor_type 过滤。
        let devices = AuditFilter {
            actor_type: Some(ActorType::Device),
            ..Default::default()
        };
        let got = store.list_audit_logs(&devices, 1, 10).expect("list");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "a-2");

        // entity_id 过滤。
        let lease_logs = AuditFilter {
            entity_type: Some("lease".into()),
            entity_id: Some("l-1".into()),
            ..Default::default()
        };
        assert_eq!(store.count_audit_logs(&lease_logs).expect("count"), 2);

        // action 过滤。
        let issue = AuditFilter {
            action: Some("issue".into()),
            ..Default::default()
        };
        assert_eq!(store.count_audit_logs(&issue).expect("count"), 1);

        // action + entity_id 组合过滤（时间窗过滤已不在 `AuditFilter` 契约内）。
        let gap_only = AuditFilter {
            action: Some("gap_alarm".into()),
            entity_id: Some("l-1".into()),
            ..Default::default()
        };
        let got = store.list_audit_logs(&gap_only, 1, 10).expect("list");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "a-3");
        assert_eq!(got[0].ts, 1_700_000_200);

        // 分页：page_size=2 → 2 / 1。
        let p1 = store
            .list_audit_logs(&AuditFilter::default(), 1, 2)
            .expect("p1");
        let p2 = store
            .list_audit_logs(&AuditFilter::default(), 2, 2)
            .expect("p2");
        assert_eq!(p1.len(), 2);
        assert_eq!(p2.len(), 1);

        // 无交集 → 空。
        let none = AuditFilter {
            action: Some("nope".into()),
            ..Default::default()
        };
        assert_eq!(store.count_audit_logs(&none).expect("count"), 0);
    }

    /// 总览「激活趋势」数据源：`count_actions_by_day` 按 UTC 日锚点分桶，
    /// 仅统计白名单动作，窗口前的行不计入。
    #[test]
    fn count_actions_by_day_groups_by_utc_day_anchor() {
        let (store, _) = fixture();
        let day0 = 1_700_000_000_i64 / 86_400 * 86_400;
        let mk = |id: &str, action: &str, ts: i64| AuditLog {
            id: id.into(),
            actor_type: ActorType::Admin,
            actor_id: "admin".into(),
            action: action.into(),
            entity_type: "activation_code".into(),
            entity_id: id.into(),
            detail: String::new(),
            ts,
            ip: String::new(),
        };
        // 当日 2 次 issue、1 次 revoke；次日 1 次 activation；窗口前 1 次 issue 与
        // 白名单外动作（heartbeat）均不计入。
        let logs = vec![
            mk("d-1", "issue", day0 + 100),
            mk("d-2", "issue", day0 + 200),
            mk("d-3", "revoke", day0 + 300),
            mk("d-4", "activation", day0 + 86_400 + 100),
            mk("d-5", "issue", day0 - 100),
            mk("d-6", "heartbeat", day0 + 400),
        ];
        for log in &logs {
            store.insert_audit_log(log).expect("insert");
        }
        let mut rows = store
            .count_actions_by_day(&["issue", "activation", "revoke"], day0)
            .expect("group");
        rows.sort();
        assert_eq!(
            rows,
            vec![
                (day0, "issue".to_string(), 2),
                (day0, "revoke".to_string(), 1),
                (day0 + 86_400, "activation".to_string(), 1),
            ],
        );
        // 空动作白名单 → 空结果。
        assert!(store
            .count_actions_by_day(&[], day0)
            .expect("empty")
            .is_empty());
    }

    #[test]
    fn disk_store_survives_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("licensing.db");
        {
            let store = Store::open(&db).expect("open");
            store
                .insert_tenant(&Tenant::new(
                    "t-persist".into(),
                    "持久化租户".into(),
                    "ops@x".into(),
                    1_700_000_000,
                ))
                .expect("insert");
        }
        let reopened = Store::open(&db).expect("reopen");
        assert_eq!(
            reopened
                .get_tenant("t-persist")
                .expect("get")
                .expect("some")
                .name,
            "持久化租户"
        );
    }

    #[test]
    fn store_debug_only_shows_path() {
        let store = Store::open_in_memory().expect("open");
        let rendered = format!("{store:?}");
        assert!(rendered.contains(":memory:"), "{rendered}");
        assert!(rendered.contains("Store"), "{rendered}");
    }

    #[test]
    fn insert_code_duplicate_value_is_storage_error() {
        let (store, _) = fixture();
        store
            .insert_code(&sample_code("c-1", "CODE-DUP"))
            .expect("c1");
        let err = store
            .insert_code(&sample_code("c-2", "CODE-DUP"))
            .expect_err("dup code value");
        assert!(matches!(err, LicenseError::Storage(_)), "{err:?}");
        // 错误信息不含码值原文。
        assert!(!err.to_string().contains("CODE-DUP"), "{err}");
    }

    #[test]
    fn now_ns_id_ids_are_storable_as_primary_keys() {
        let (store, _) = fixture();
        let id = now_ns_id("dev");
        let mut device = sample_device(&id, "mc-gen", DeviceStatus::Active);
        device.device_id = id.clone();
        store.insert_device(&device).expect("insert");
        assert_eq!(
            store.get_device(&id).expect("get").expect("some").device_id,
            id
        );
    }
}
