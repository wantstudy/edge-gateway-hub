//! 统一 SQLite 迁移框架 + 配置迁移 / 坏配置回退原语（task 55）。
//!
//! ## 职责边界
//! - **做**：以 `PRAGMA user_version` 为**唯一权威版本源**的 SQLite 迁移框架；
//!   每条迁移**单事务**、失败回滚且 `user_version` 不前进；迁移前后写 `tracing`
//!   审计日志（对齐项目既有风格）+ `schema_migrations` 人类可读账本；
//!   forward-only + `integrity_check` 一致性断言；配置升级链 / [`LoadOutcome`]
//!   三态语义 / `backup_before_rewrite` 原子备份等**原语**（本 task 不接线）。
//! - **不做**：调用方接线（telemetry_store / mgmt 后续自行接入）、北向、降级迁移。
//!
//! ## 关键设计决策
//!
//! 1. **`user_version` 是唯一权威**：`schema_migrations` 表只是**人类可读账本**，
//!    权威判定永远读 `PRAGMA user_version`。二者一致性由 [`integrity_check`] 校验
//!    （账本行必须恰为 `1..=user_version`）。
//! 2. **Forward-only**：库版本高于二进制支持的最新版本时**报错拒绝打开**
//!    （「旧程序 + 新库」场景，降级写入可能损坏数据，绝不静默处理）。
//! 3. **每条迁移一个事务**：迁移 SQL + 账本 INSERT + `user_version` 前进
//!    **同事务提交**；任何一步失败 → 整体回滚，`user_version` 保持不变，
//!    不会出现「表建了一半、版本号却前进了」的半成品状态。
//! 4. **零 panic**：所有错误收敛为 `DaemonError::{StorageError(4000), ConfigError(2000)}`，
//!    与 `telemetry_store.rs` 同风格；非测试代码无 `unwrap` / `expect` / `panic`。
//! 5. **依赖白名单**：仅用现有依赖（rusqlite / tracing），未引入新 crate。

use std::fmt;
use std::fs::File;
use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};

use crate::error::{DaemonError, DaemonResult};

// ---------------------------------------------------------------------------
// 内置迁移注册表
// ---------------------------------------------------------------------------

/// v1 迁移 SQL：建 `schema_migrations` 人类可读账本表。
///
/// 权威版本源是 `PRAGMA user_version`；本表仅提供「谁在何时升到哪版」的
/// 人工审计视图，一致性由 [`integrity_check`] 断言。
pub const V1_LEDGER_SQL: &str = "
CREATE TABLE IF NOT EXISTS schema_migrations (
    version     INTEGER PRIMARY KEY,
    description TEXT    NOT NULL,
    applied_at  INTEGER NOT NULL
);
";

/// 单条迁移的执行体：静态 SQL 批，或自定义闭包（如需要程序逻辑的数据搬运）。
pub enum MigrationStep {
    /// 多语句 SQL 批（事务内 `execute_batch` 执行）。
    Sql(&'static str),
    /// 自定义迁移函数（同样运行在迁移事务内，失败即整体回滚）。
    Custom(fn(&Connection) -> DaemonResult<()>),
}

impl fmt::Debug for MigrationStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // SQL 内容不打印（可能很长），仅标注种类。
            Self::Sql(_) => f.write_str("Sql(<batch>)"),
            Self::Custom(_) => f.write_str("Custom(<fn>)"),
        }
    }
}

/// 一条迁移：版本号 + 描述 + 执行体。
#[derive(Debug)]
pub struct Migration {
    /// 目标版本号（迁移完成后 `user_version` 应等于此值；必须 > 0，注册表内严格递增）。
    pub version: u32,
    /// 人类可读描述（写入账本与审计日志）。
    pub description: &'static str,
    /// 执行体。
    pub step: MigrationStep,
}

/// 内置迁移注册表（v1 起；后续版本在此追加，**只增不改**）。
#[must_use]
pub fn builtin_registry() -> &'static [Migration] {
    &[Migration {
        version: 1,
        description: "create schema_migrations ledger table",
        step: MigrationStep::Sql(V1_LEDGER_SQL),
    }]
}

/// 内置注册表的最新版本号。
#[must_use]
pub fn builtin_latest_version() -> u32 {
    builtin_registry()
        .last()
        .map_or(0, |m: &Migration| m.version)
}

/// v2 迁移 SQL（task 26 安全审计）：`audit_log` 追加链表 + `audit_meta` 派生盐表。
///
/// - `audit_log`：一行一条安全审计事件；**追加写**由两个触发器强制——
///   UPDATE / DELETE 一律 `RAISE(ABORT)`（QA 场景「尝试删除日志 → 断言失败」
///   的落点；链完整性由 `audit.rs` 的 HMAC-SHA256 哈希链校验承担）；
/// - `audit_meta`：链盐（`chain_salt`）等派生材料的持久化（重开库可复算链密钥）；
/// - 时间/事件索引服务 mgmt 远程拉取端点的时间窗与事件类型过滤。
pub const V2_AUDIT_SQL: &str = "
CREATE TABLE IF NOT EXISTS audit_log (
    seq        INTEGER PRIMARY KEY,
    ts_ns      INTEGER NOT NULL,
    actor      TEXT    NOT NULL,
    event      TEXT    NOT NULL,
    outcome    TEXT    NOT NULL,
    detail     TEXT    NOT NULL,
    prev_hash  TEXT    NOT NULL,
    entry_hash TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS audit_log_ts_idx    ON audit_log(ts_ns);
CREATE INDEX IF NOT EXISTS audit_log_event_idx ON audit_log(event);
CREATE TABLE IF NOT EXISTS audit_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TRIGGER IF NOT EXISTS audit_log_no_update BEFORE UPDATE ON audit_log
BEGIN
    SELECT RAISE(ABORT, 'audit_log is append-only: update rejected');
END;
CREATE TRIGGER IF NOT EXISTS audit_log_no_delete BEFORE DELETE ON audit_log
BEGIN
    SELECT RAISE(ABORT, 'audit_log is append-only: delete rejected');
END;
";

/// 审计库迁移注册表（v1 账本 + v2 审计表）。
///
/// 与 `builtin_registry()`（只到 v1）分离：telemetry.db / queue.db 继续用内置
/// 注册表不受影响；审计库（audit.db）用本注册表独立迁移（task 26）。
#[must_use]
pub fn audit_registry() -> Vec<Migration> {
    vec![
        Migration {
            version: 1,
            description: "create schema_migrations ledger table",
            step: MigrationStep::Sql(V1_LEDGER_SQL),
        },
        Migration {
            version: 2,
            description: "create audit_log append-only chain table and audit_meta",
            step: MigrationStep::Sql(V2_AUDIT_SQL),
        },
    ]
}

// ---------------------------------------------------------------------------
// 执行报告
// ---------------------------------------------------------------------------

/// 单条已应用迁移的记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedMigration {
    /// 版本号。
    pub version: u32,
    /// 描述（与注册表一致）。
    pub description: &'static str,
}

/// `run_migrations` 的执行报告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedReport {
    /// 迁移前的 `user_version`。
    pub from_version: u32,
    /// 迁移后的 `user_version`（无迁移时等于 `from_version`）。
    pub to_version: u32,
    /// 本次实际应用的迁移（按版本升序；幂等重跑时为空）。
    pub applied: Vec<AppliedMigration>,
}

impl AppliedReport {
    /// 本次是否为 no-op（无迁移被应用）。
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.applied.is_empty()
    }
}

// ---------------------------------------------------------------------------
// 核心执行
// ---------------------------------------------------------------------------

/// 对连接执行内置注册表的全部待应用迁移（幂等）。
///
/// # Errors
/// 见 [`run_migrations_with`]。
pub fn run_migrations(conn: &Connection) -> DaemonResult<AppliedReport> {
    run_migrations_with(conn, builtin_registry())
}

/// 对连接执行给定注册表的全部待应用迁移（幂等，forward-only）。
///
/// ## 幂等语义
/// 当前 `user_version >= 目标版本` 的迁移全部跳过；重跑返回空 `applied`。
///
/// ## 事务语义
/// 每条迁移独占一个事务：迁移 SQL + 账本记录 + `user_version` 前进同事务提交；
/// 失败整体回滚，`user_version` 不前进、半成品表被撤销。
///
/// # Errors
/// - 注册表版本非严格递增 / 含 0 → `ConfigError`（2000）；
/// - 库版本高于注册表最新版（旧程序开新库）→ `StorageError`（4000），拒绝降级；
/// - 任一迁移执行失败 → `StorageError`（4000），该迁移事务已回滚。
pub fn run_migrations_with(
    conn: &Connection,
    registry: &[Migration],
) -> DaemonResult<AppliedReport> {
    validate_registry(registry)?;
    let from = current_version(conn)?;
    let latest = registry.last().map_or(0, |m: &Migration| m.version);
    if from > latest {
        // Forward-only：旧二进制遇到新库，绝不降级、绝不静默。
        return Err(storage_err(format!(
            "database user_version {from} is newer than supported version {latest}; \
             forward-only migrations forbid downgrade"
        )));
    }

    let mut report = AppliedReport {
        from_version: from,
        to_version: from,
        applied: Vec::new(),
    };
    for migration in registry {
        if migration.version <= from {
            continue; // 已应用，幂等跳过。
        }
        apply_one(conn, migration)?;
        report.applied.push(AppliedMigration {
            version: migration.version,
            description: migration.description,
        });
        report.to_version = migration.version;
    }
    Ok(report)
}

/// 应用单条迁移（一个事务，含账本与版本号前进；失败整体回滚）。
fn apply_one(conn: &Connection, migration: &Migration) -> DaemonResult<()> {
    tracing::info!(
        target: "daemon::migrations",
        version = migration.version,
        description = migration.description,
        "migration: applying"
    );
    // unchecked_transaction：仅需 &Connection（rusqlite 的 transaction() 要求 &mut）。
    let tx = conn.unchecked_transaction().map_err(map_sqlite)?;
    match &migration.step {
        MigrationStep::Sql(sql) => tx.execute_batch(sql).map_err(map_sqlite)?,
        MigrationStep::Custom(func) => func(&tx)?,
    }
    // 账本 + 版本号与迁移 SQL 同事务：回滚时三者一起回滚。
    tx.execute(
        "INSERT OR REPLACE INTO schema_migrations(version, description, applied_at) \
         VALUES(?1, ?2, ?3)",
        params![
            i64::from(migration.version),
            migration.description,
            unix_secs_i64()
        ],
    )
    .map_err(map_sqlite)?;
    tx.pragma_update(None, "user_version", i64::from(migration.version))
        .map_err(map_sqlite)?;
    tx.commit().map_err(map_sqlite)?;
    tracing::info!(
        target: "daemon::migrations",
        version = migration.version,
        "migration: applied"
    );
    Ok(())
}

/// 注册表合法性校验：版本 > 0 且严格递增。
fn validate_registry(registry: &[Migration]) -> DaemonResult<()> {
    let mut prev = 0u32;
    for migration in registry {
        if migration.version == 0 {
            return Err(config_err(format!(
                "migration version must be > 0 (got 0: `{}`)",
                migration.description
            )));
        }
        if migration.version <= prev {
            return Err(config_err(format!(
                "migration versions must be strictly ascending ({} after {prev})",
                migration.version
            )));
        }
        prev = migration.version;
    }
    Ok(())
}

/// 读取当前 `user_version`（唯一权威版本源）。
///
/// # Errors
/// SQLite 读失败 → `StorageError`（4000）。
pub fn current_version(conn: &Connection) -> DaemonResult<u32> {
    let raw: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(map_sqlite)?;
    Ok(u32::try_from(raw.max(0)).unwrap_or(u32::MAX))
}

/// UNIX 秒（时钟异常时退化为 0，绝不 panic）。
fn unix_secs_i64() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

// ---------------------------------------------------------------------------
// 一致性自检
// ---------------------------------------------------------------------------

/// `integrity_check` 的结果报告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrityReport {
    /// 当前 `user_version`。
    pub user_version: u32,
    /// 内置注册表最新版本号。
    pub registry_latest: u32,
    /// 账本中记录的已应用版本（升序）。
    pub ledger_versions: Vec<u32>,
}

/// 库完整性自检（应在打开 / 迁移后调用）。
///
/// 检查项：
/// 1. `PRAGMA integrity_check` 必须返回 `ok`（页级损坏即失败）；
/// 2. `user_version` 不得超过内置注册表最新版（旧程序开新库 → 拒绝）；
/// 3. `user_version > 0` 时，`schema_migrations` 账本必须存在且行集恰为
///    `1..=user_version`（账本与权威版本源一致）。
///
/// # Errors
/// 任一检查失败 → `StorageError`（4000）。
pub fn integrity_check(conn: &Connection) -> DaemonResult<IntegrityReport> {
    // 1) 页级完整性。
    let status: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(map_sqlite)?;
    if !status.eq_ignore_ascii_case("ok") {
        return Err(storage_err(format!(
            "sqlite integrity_check failed: {status}"
        )));
    }

    // 2) 版本不超前于二进制（forward-only）。
    let user_version = current_version(conn)?;
    let registry_latest = builtin_latest_version();
    if user_version > registry_latest {
        return Err(storage_err(format!(
            "user_version {user_version} exceeds supported {registry_latest}; \
             database was written by a newer binary"
        )));
    }

    // 3) 账本一致性。
    let mut ledger_versions = Vec::new();
    if user_version >= 1 {
        let table_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'table' AND name = 'schema_migrations'",
                [],
                |row| row.get(0),
            )
            .map_err(map_sqlite)?;
        if table_count == 0 {
            return Err(storage_err(
                "user_version > 0 but schema_migrations ledger table is missing",
            ));
        }
        let mut stmt = conn
            .prepare("SELECT version FROM schema_migrations ORDER BY version")
            .map_err(map_sqlite)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, i64>(0))
            .map_err(map_sqlite)?;
        for row in rows {
            let raw = row.map_err(map_sqlite)?;
            ledger_versions.push(u32::try_from(raw.max(0)).unwrap_or(u32::MAX));
        }
        let expected: Vec<u32> = (1..=user_version).collect();
        if ledger_versions != expected {
            return Err(storage_err(format!(
                "schema_migrations ledger {ledger_versions:?} does not match \
                 expected 1..={user_version}"
            )));
        }
    }

    Ok(IntegrityReport {
        user_version,
        registry_latest,
        ledger_versions,
    })
}

// ---------------------------------------------------------------------------
// 配置迁移 / 坏配置回退原语（骨架，本 task 不接线）
// ---------------------------------------------------------------------------

/// 旧版配置文本 + 其版本号（迁移链的输入）。
///
/// ## 坏配置回退的总体语义
/// 打开配置时先按 `from_version` 走升级链；任何一步失败都**不得**就地改写
/// 原文件——先 [`backup_before_rewrite`] 备份，再由调用方按 [`LoadOutcome`]
/// 三态决定继续 / 降级运行 / 进入安全模式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrateConfig {
    /// 配置文本所属的历史版本号（0 表示无法识别来源版本）。
    pub from_version: u32,
    /// 旧版 TOML 原文（迁移前保持只读，绝不在此类型内改写文件）。
    pub raw_toml: String,
}

impl MigrateConfig {
    /// 构造。
    #[must_use]
    pub fn new(from_version: u32, raw_toml: impl Into<String>) -> Self {
        Self {
            from_version,
            raw_toml: raw_toml.into(),
        }
    }

    /// 沿升级链把旧配置文本迁到链上可抵达的最新版本。
    ///
    /// # Errors
    /// 链上没有以 `from_version` 为起点的步骤 → `ConfigError`（2000）；
    /// 某步迁移函数失败 → 透传其错误（原文本未被改动）。
    pub fn upgrade_with(
        &self,
        chain: &ConfigMigrationChain,
    ) -> DaemonResult<ConfigMigrationOutcome> {
        chain.apply(&self.raw_toml, self.from_version)
    }
}

/// 单步配置升级的最小契约。
pub trait ConfigUpgradeStep {
    /// 该步的输入版本。
    fn source_version(&self) -> u32;
    /// 该步的输出版本（约定 `to = from + 1`，forward-only）。
    fn target_version(&self) -> u32;
    /// 把旧文本升级为新文本（纯函数：不改文件、失败返回错误）。
    ///
    /// # Errors
    /// 文本不兼容 / 字段缺失 / 解析失败 → `ConfigError`（2000）。
    fn upgrade(&self, raw_toml: &str) -> DaemonResult<String>;
}

/// 函数指针版升级步骤（最常用的骨架实现；测试与简单迁移直接用它）。
pub struct FnUpgradeStep {
    from_v: u32,
    to_v: u32,
    func: fn(&str) -> DaemonResult<String>,
}

impl FnUpgradeStep {
    /// 构造（约定 `to_version == from_version + 1`，构造即断言，不符为 `None`）。
    #[must_use]
    pub fn new(
        from_version: u32,
        to_version: u32,
        func: fn(&str) -> DaemonResult<String>,
    ) -> Option<Self> {
        if to_version == from_version.checked_add(1)? {
            Some(Self {
                from_v: from_version,
                to_v: to_version,
                func,
            })
        } else {
            None
        }
    }
}

impl ConfigUpgradeStep for FnUpgradeStep {
    fn source_version(&self) -> u32 {
        self.from_v
    }

    fn target_version(&self) -> u32 {
        self.to_v
    }

    fn upgrade(&self, raw_toml: &str) -> DaemonResult<String> {
        (self.func)(raw_toml)
    }
}

/// 配置升级链：按版本顺序串联多个 [`ConfigUpgradeStep`]，
/// 把 `from_version` 的旧文本一路升到链上可抵达的最新版本。
#[derive(Default)]
pub struct ConfigMigrationChain {
    steps: Vec<Box<dyn ConfigUpgradeStep>>,
}

/// 升级链执行结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigMigrationOutcome {
    /// 迁移后的配置文本。
    pub toml: String,
    /// 迁移后到达的版本号。
    pub to_version: u32,
    /// 实际走过的步骤（输入版本序列，升序）。
    pub applied_steps: Vec<u32>,
}

impl ConfigMigrationChain {
    /// 空链。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加一步（forward-only：版本必须严格递增）。
    ///
    /// # Errors
    /// 与已有步骤版本重叠 / 倒退 → `ConfigError`（2000）。
    pub fn push(&mut self, step: Box<dyn ConfigUpgradeStep>) -> DaemonResult<()> {
        let from = step.source_version();
        if let Some(last) = self.steps.last() {
            // 相邻步骤共享边界版本（前一步的输出 = 后一步的输入）是正常的；
            // 只有越过边界（from < 上一步输出）才算重叠 / 倒退。
            if from < last.target_version() {
                return Err(config_err(format!(
                    "config upgrade chain must be strictly ascending (new step starts at {from}, \
                     chain already reaches {})",
                    last.target_version()
                )));
            }
        }
        self.steps.push(step);
        Ok(())
    }

    /// 链上可抵达的最高输出版本（空链为 `None`）。
    #[must_use]
    pub fn latest_version(&self) -> Option<u32> {
        self.steps.last().map(|s| s.target_version())
    }

    /// 从 `from_version` 起沿链升级。
    ///
    /// # Errors
    /// - 链上没有以 `from_version` 为起点的步骤、且 `from_version` 不是链顶
    ///   （版本有空洞或超前于本二进制）→ `ConfigError`（2000）；
    /// - 任一步失败 → 透传。
    pub fn apply(&self, raw_toml: &str, from_version: u32) -> DaemonResult<ConfigMigrationOutcome> {
        let mut current = raw_toml.to_string();
        let mut version = from_version;
        let mut applied_steps = Vec::new();
        loop {
            let found = self.steps.iter().find(|s| s.source_version() == version);
            match found {
                Some(step) => {
                    current = step.upgrade(&current)?;
                    version = step.target_version();
                    applied_steps.push(step.source_version());
                }
                None => {
                    // 已站在链顶（或空链）：无事可做，视为 no-op 成功；
                    // 其余情况（版本空洞 / 超前）按坏配置处理。
                    match self.latest_version() {
                        Some(latest) if latest == version => break,
                        None => break,
                        Some(latest) => {
                            return Err(config_err(format!(
                                "no config upgrade path from version {version} \
                                 (chain covers up to {latest})"
                            )));
                        }
                    }
                }
            }
        }
        Ok(ConfigMigrationOutcome {
            toml: current,
            to_version: version,
            applied_steps,
        })
    }
}

// ---------------------------------------------------------------------------
// LoadOutcome：坏配置 / 异常时的三态装载语义（骨架，不接线）
// ---------------------------------------------------------------------------

/// 配置装载结果三态。
///
/// ## 语义（供接线方遵守）
/// - [`LoadOutcome::Ok`]：配置合法，正常运行（采集 + 北向全开）。
/// - [`LoadOutcome::Recovered`]：配置**有缺陷但可自动修复**（如旧版本字段
///   迁移补齐、缺失项取默认值）。修复后照常运行，但 `warnings` 必须**逐条**
///   落审计日志（对齐项目「不静默吞异常」纪律），并提示用户改配置文件。
/// - [`LoadOutcome::SafeMode`]：配置**坏到无法安全运行**（解析失败 / 关键
///   字段缺失 / 迁移失败）。**安全模式语义**：
///   1. **仅本地采集**：采集引擎照常工作，本地遥测归档（telemetry.db）继续；
///   2. **停北向**：不上传 / 不转发到任何远端（防止坏配置把脏数据推上云）；
///   3. **须审计告警**：进入 / 退出安全模式都必须打告警级审计日志并通知运维；
///   4. 原配置文件**绝不**被就地改写；修复只能基于 [`backup_before_rewrite`]
///      备份 + 用户显式确认后写回。
pub enum LoadOutcome<T> {
    /// 配置合法：全功能运行。
    Ok(T),
    /// 配置已自动修复：照常运行 + 逐条审计告警。
    Recovered {
        /// 修复后的配置。
        config: T,
        /// 修复动作说明（每条都会被要求落审计）。
        warnings: Vec<String>,
    },
    /// 配置损坏：进入安全模式（仅本地采集、停北向、须审计告警）。
    SafeMode {
        /// 进入安全模式的原因（写审计日志用）。
        reason: String,
    },
}

impl<T> LoadOutcome<T> {
    /// 是否处于安全模式。
    #[must_use]
    pub fn is_safe_mode(&self) -> bool {
        matches!(self, Self::SafeMode { .. })
    }

    /// 是否发生了自动修复（`Recovered`）。
    #[must_use]
    pub fn is_recovered(&self) -> bool {
        matches!(self, Self::Recovered { .. })
    }
}

impl<T: fmt::Debug> fmt::Debug for LoadOutcome<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ok(config) => f.debug_tuple("Ok").field(config).finish(),
            Self::Recovered { config, warnings } => f
                .debug_struct("Recovered")
                .field("config", config)
                .field("warnings", warnings)
                .finish(),
            Self::SafeMode { reason } => {
                f.debug_struct("SafeMode").field("reason", reason).finish()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 备份命名（新格式内嵌可读时间 + 旧格式向后兼容）
// ---------------------------------------------------------------------------
//
// **新建**备份统一采用新格式：
//   `{config}.YYYYMMDD-HHmmss-NNN.bak`
//   · 内嵌时刻统一 **东八区（UTC+8，无夏令时）**，全进程一致；
//   · `YYYYMMDD-HHmmss` 为 UTC+8 墙钟时间；`NNN` 为同一秒内从 `000` 起的 3 位
//     零填充序号（保证唯一，且字典序 = 时间序）；
//   · 例：`config.toml.20260927-011231-000.bak`。
//
// 旧格式（历史遗留，**继续识别**：可列出、可恢复、可参与 retention 清理）：
//   `{config}.bak-<unix秒>` / `{config}.bak-<unix秒>-<序号>`
//   / `{config}.bak-manual-<epoch_ms>` / `{config}.bak-periodic-<epoch_ms>`。

/// 东八区相对 UTC 的秒偏移（备份文件名内嵌时刻统一 UTC+8）。
const UTC8_OFFSET_SECS: i64 = 8 * 3600;

/// 结构解析新格式 token（`YYYYMMDD-HHmmss` 或 `YYYYMMDD-HHmmss-NNN`）→ epoch 毫秒。
///
/// 序号 `NNN` 作为毫秒尾数并入（同秒内单调递增）；任一字段结构 / 范围不合法 → `None`。
fn parse_readable_token(token: &str) -> Option<u64> {
    let (date, rest) = token.split_once('-')?;
    if date.len() != 8 || !date.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (time, seq) = match rest.split_once('-') {
        Some((time, seq)) => (time, Some(seq)),
        None => (rest, None),
    };
    if time.len() != 6 || !time.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let seq = match seq {
        None => 0u64,
        Some(raw) => {
            if raw.len() != 3 || !raw.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            raw.parse::<u64>().ok()?
        }
    };
    let year = date.get(0..4)?.parse::<i64>().ok()?;
    let month = date.get(4..6)?.parse::<i64>().ok()?;
    let day = date.get(6..8)?.parse::<i64>().ok()?;
    let hour = time.get(0..2)?.parse::<i64>().ok()?;
    let minute = time.get(2..4)?.parse::<i64>().ok()?;
    let second = time.get(4..6)?.parse::<i64>().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let wall = days
        .saturating_mul(86_400)
        .saturating_add(hour * 3600 + minute * 60 + second);
    let epoch = wall.saturating_sub(UTC8_OFFSET_SECS);
    if epoch < 0 {
        return None;
    }
    Some(
        u64::try_from(epoch)
            .ok()?
            .saturating_mul(1000)
            .saturating_add(seq),
    )
}

/// 公历 `YYYY-MM-DD` → 自 1970-01-01 起的天数（Howard Hinnant 算法，纯整数）。
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 自 1970-01-01 起的天数 → `(年, 月, 日)`（Howard Hinnant 算法，纯整数）。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { y + 1 } else { y }, month, day)
}

/// epoch 秒 → UTC+8 `YYYYMMDD-HHmmss`（按 `div_euclid` 处理负值，绝不 panic）。
fn format_utc8_token(epoch_secs: i64) -> String {
    let wall = epoch_secs.saturating_add(UTC8_OFFSET_SECS);
    let days = wall.div_euclid(86_400);
    let rem = wall.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = rem / 3600;
    let minute = (rem % 3600) / 60;
    let second = rem % 60;
    format!("{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}")
}

/// 判定 `name` 是否本服务自产的备份文件名（新旧两种命名都算；临时文件不算）。
pub fn is_backup_file_name(name: &str, config_file_name: &str) -> bool {
    if name.contains(".tmp") {
        return false;
    }
    let legacy_prefix = format!("{config_file_name}.bak-");
    if name
        .strip_prefix(legacy_prefix.as_str())
        .is_some_and(|rest| !rest.is_empty())
    {
        return true;
    }
    let readable_prefix = format!("{config_file_name}.");
    name.strip_prefix(readable_prefix.as_str())
        .and_then(|rest| rest.strip_suffix(".bak"))
        .and_then(parse_readable_token)
        .is_some()
}

/// 备份排序键（**epoch 毫秒**，升序 = 时间升序）。
///
/// 新旧两种命名统一映射到同一时间轴；不可解析 → `u64::MAX`（视为最新，retention 保留）。
pub fn backup_sort_key(name: &str, config_file_name: &str) -> u64 {
    if name.contains(".tmp") {
        return u64::MAX;
    }
    let legacy_prefix = format!("{config_file_name}.bak-");
    if let Some(rest) = name.strip_prefix(legacy_prefix.as_str()) {
        // epoch 毫秒命名（manual / periodic）。
        if let Some(ms) = rest
            .strip_prefix("manual-")
            .or_else(|| rest.strip_prefix("periodic-"))
        {
            return parse_leading_u64(ms).unwrap_or(u64::MAX);
        }
        // `bak-<unix秒>` / `bak-<unix秒>-<序号>`。
        let digits = leading_digits(rest);
        let Ok(secs) = digits.parse::<u64>() else {
            return u64::MAX;
        };
        let seq = rest[digits.len()..]
            .strip_prefix('-')
            .and_then(parse_leading_u64)
            .unwrap_or(0)
            .min(999);
        return secs.saturating_mul(1000).saturating_add(seq);
    }
    let readable_prefix = format!("{config_file_name}.");
    name.strip_prefix(readable_prefix.as_str())
        .and_then(|rest| rest.strip_suffix(".bak"))
        .and_then(parse_readable_token)
        .unwrap_or(u64::MAX)
}

/// 取字符串的前导 ASCII 数字段。
fn leading_digits(text: &str) -> String {
    text.chars().take_while(char::is_ascii_digit).collect()
}

/// 解析字符串开头的前导 ASCII 数字段为 `u64`（无数字前缀 → `None`）。
fn parse_leading_u64(text: &str) -> Option<u64> {
    let digits = leading_digits(text);
    if digits.is_empty() {
        return None;
    }
    digits.parse::<u64>().ok()
}

/// 为下一次备份构造**唯一**的新格式路径（只做命名，不落盘）。
///
/// 同一秒内扫描已存在的 `{base}-NNN.bak` 取 `max(N)+1`；仍冲突时继续 `+1`
/// （有界 1000 次，绝不 panic）。
pub fn next_backup_path(dir: &Path, config_file_name: &str) -> PathBuf {
    let base = format!(
        "{config_file_name}.{}",
        format_utc8_token(unix_secs_i64())
    );
    let scan_prefix = format!("{base}-");
    let mut next: u32 = 0;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let Some(fname) = entry.file_name().to_str().map(ToString::to_string) else {
                continue;
            };
            let Some(seq) = fname
                .strip_prefix(scan_prefix.as_str())
                .and_then(|rest| rest.strip_suffix(".bak"))
            else {
                continue;
            };
            if seq.len() == 3 && seq.bytes().all(|b| b.is_ascii_digit()) {
                if let Ok(value) = seq.parse::<u32>() {
                    next = next.max(value.saturating_add(1));
                }
            }
        }
    }
    let mut candidate = dir.join(format!("{base}-{next:03}.bak"));
    let mut tries = 0u32;
    while candidate.exists() && tries < 1000 {
        next = next.saturating_add(1);
        tries += 1;
        candidate = dir.join(format!("{base}-{next:03}.bak"));
    }
    candidate
}

// ---------------------------------------------------------------------------
// 原子备份
// ---------------------------------------------------------------------------

/// 在改写配置文件前做原子备份。
///
/// ## 原子性
/// 先把内容写入同目录临时文件（`.tmp` 后缀）并 `sync_all` 落盘，再
/// `rename` 到最终备份名——rename 在同一文件系统内是原子的，故备份文件
/// 要么完整存在、要么不存在，绝不会出现写了一半的备份。
///
/// 备份命名：见本模块「备份命名」小节（新格式 `{config}.YYYYMMDD-HHmmss-NNN.bak`，
/// 时刻统一 UTC+8）。旧格式 `{config}.bak-<unix秒>` 仍可被识别 / 恢复。
///
/// # Errors
/// - `path` 不存在 / 不是普通文件 → `ConfigError`（2000）；
/// - 读 / 写 / 落盘 / rename 失败 → `StorageError`（4000），并清理半成品临时文件。
pub fn backup_before_rewrite(path: &Path) -> DaemonResult<PathBuf> {
    if !path.is_file() {
        return Err(config_err(format!(
            "backup target is not an existing regular file: {}",
            path.display()
        )));
    }
    let data = std::fs::read(path)
        .map_err(|e| storage_err(format!("backup: read {}: {e}", path.display())))?;

    let dir = path
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config")
        .to_string();

    // 最终备份名（新格式，同秒内唯一）。
    let backup = next_backup_path(&dir, &file_name);

    // 临时文件 + fsync + 原子 rename。
    let backup_file_name = backup
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config.bak");
    let tmp = dir.join(format!("{backup_file_name}.tmp-{}", std::process::id()));
    let write_result = (|| -> std::io::Result<()> {
        let mut file = File::create(&tmp)?;
        file.write_all(&data)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(storage_err(format!(
            "backup: write temp {}: {e}",
            tmp.display()
        )));
    }
    if let Err(e) = std::fs::rename(&tmp, &backup) {
        let _ = std::fs::remove_file(&tmp);
        return Err(storage_err(format!(
            "backup: rename {} -> {}: {e}",
            tmp.display(),
            backup.display()
        )));
    }
    Ok(backup)
}

/// 备份保留策略清理：只删**本服务自产的**备份（新格式
/// `{file_name}.YYYYMMDD-HHmmss-NNN.bak` 与旧格式 `{file_name}.bak-*` 都算，
/// 与 [`backup_before_rewrite`] / [`is_backup_file_name`] 同一口径；`.tmp-`
/// 半成品一并纳入清理范围——同样只可能是本服务产物）。按名字内嵌的**时刻**
/// 升序保留最新 `retention` 份，超出部分从最旧开始删除。
///
/// - 排序键 = [`backup_sort_key`]（统一 epoch 毫秒：新格式解析 `YYYYMMDD-HHmmss`
///   为 UTC+8 墙钟；旧格式 `bak-<unix秒>` / `bak-manual-<ms>` / `bak-periodic-<ms>`
///   / `bak-<秒>-<序号>` 全部命中；同一前缀下秒/毫秒混排时字典序 ≠ 时间序，
///   故必须按数值比较）；
/// - 不可解析的名字排序键取 `u64::MAX`（视为最新，保留——fail-safe）；
/// - `retention == 0` → 不清理（调用方语义：0 = 保留无限份）；
/// - 单个文件删除失败 → 记 warn 并跳过该文件继续（绝不因清理失败阻断写路径）；
/// - **绝不匹配其它命名模式**：用户自建备份（如 `config.toml.mybak`）不受影响。
///
/// 返回实际删除的文件数。
/// 备份扫描目录解析：相对裸文件名（如 `config.toml`）的 `parent()` 是空串，
/// `read_dir("")` 会失败 → 清理/计数双双静默 no-op（真机缺陷实证）。此处统一
/// 把空 parent 回退为 `"."`（进程 cwd），与「相对路径相对 cwd 解析」语义一致。
fn backup_search_dir(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .map(Path::to_path_buf)
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn enforce_backup_retention(config_path: &Path, retention: u32) -> usize {
    if retention == 0 {
        return 0;
    }
    let dir = backup_search_dir(config_path);
    let file_name = config_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let mut backups: Vec<(u64, String)> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_file())
        .filter_map(|entry| entry.file_name().to_str().map(ToString::to_string))
        .filter(|name| is_backup_file_name(name, file_name))
        .map(|name| (backup_sort_key(&name, file_name), name))
        .collect();
    backups.sort();
    let excess = backups.len().saturating_sub(retention as usize);
    let mut removed = 0usize;
    for (_, name) in backups.into_iter().take(excess) {
        match std::fs::remove_file(dir.join(&name)) {
            Ok(()) => removed += 1,
            Err(err) => tracing::warn!(
                file = %name,
                error = %err,
                "backup retention: failed to remove oldest backup; skipped"
            ),
        }
    }
    removed
}

/// 统计本服务自产备份文件数（新旧两种命名，与 [`enforce_backup_retention`]
/// 同一匹配口径；目录不可读 → 0）。
pub fn count_backup_files(config_path: &Path) -> usize {
    let dir = backup_search_dir(config_path);
    let file_name = config_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_file())
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|n| is_backup_file_name(n, file_name))
        })
        .count()
}

// ---------------------------------------------------------------------------
// 错误工具（与 telemetry_store.rs 同风格）
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{ERR_CONFIG, ERR_STORAGE};

    /// 新建内存外临时库连接（文件库，便于测 user_version 持久性）。
    fn open_conn(dir: &Path) -> Connection {
        Connection::open(dir.join("migrate-test.db")).expect("open db")
    }

    /// v1 + v2（建表 t2）的合法测试注册表。
    fn registry_two() -> Vec<Migration> {
        vec![
            Migration {
                version: 1,
                description: "v1 ledger",
                step: MigrationStep::Sql(V1_LEDGER_SQL),
            },
            Migration {
                version: 2,
                description: "v2 create t2",
                step: MigrationStep::Sql("CREATE TABLE t2 (id INTEGER);"),
            },
        ]
    }

    /// QA Happy：新库跑迁移 → v1 应用、user_version=1、账本 1 行。
    #[test]
    fn fresh_db_applies_v1_and_records_ledger() {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = open_conn(dir.path());

        let report = run_migrations(&conn).expect("run");
        assert_eq!(report.from_version, 0);
        assert_eq!(report.to_version, 1);
        assert_eq!(report.applied.len(), 1);
        assert_eq!(report.applied[0].version, 1);

        assert_eq!(current_version(&conn).expect("version"), 1);
        let (desc, at): (String, i64) = conn
            .query_row(
                "SELECT description, applied_at FROM schema_migrations WHERE version = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("ledger row");
        assert_eq!(desc, "create schema_migrations ledger table");
        assert!(at >= 0, "applied_at 必须是合法 UNIX 秒");
    }

    /// 幂等：重跑全部跳过，账本不重复，报告为 no-op。
    #[test]
    fn run_migrations_is_idempotent_no_op() {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = open_conn(dir.path());
        run_migrations_with(&conn, &registry_two()).expect("first run");

        let second = run_migrations_with(&conn, &registry_two()).expect("second run");
        assert!(second.is_noop(), "重跑必须 no-op: {second:?}");
        assert_eq!(second.from_version, 2);
        assert_eq!(second.to_version, 2);

        let ledger_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .expect("count");
        assert_eq!(ledger_rows, 2, "账本不得重复插入");
        // 第三跑依旧 no-op（用同一注册表；内置注册表只到 v1，不适用）。
        assert!(run_migrations_with(&conn, &registry_two())
            .expect("third run")
            .is_noop());
    }

    /// 事务回滚：v2 迁移失败 → user_version 不动、v2 的表未被建立。
    #[test]
    fn failed_migration_rolls_back_user_version_and_tables() {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = open_conn(dir.path());
        let mut registry = registry_two();
        registry.push(Migration {
            version: 3,
            description: "v3 broken",
            // 先建表、再撞一个不存在的表：v3 必失败且整体回滚。
            step: MigrationStep::Sql(
                "CREATE TABLE t3 (id INTEGER); INSERT INTO no_such_table VALUES (1);",
            ),
        });

        let err = run_migrations_with(&conn, &registry).expect_err("v3 must fail");
        assert_eq!(err.error_code(), ERR_STORAGE);
        // user_version 停在 v2，v1/v2 不受拖累。
        assert_eq!(current_version(&conn).expect("version"), 2);
        // t3 表必须被回滚掉（半成品不可见）。
        let t3: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='t3'",
                [],
                |r| r.get(0),
            )
            .expect("probe t3");
        assert_eq!(t3, 0, "失败迁移建的表必须随事务回滚");
        // 幂等修复后可继续（把坏迁移换成好的重跑）。
        registry[2] = Migration {
            version: 3,
            description: "v3 fixed",
            step: MigrationStep::Sql("CREATE TABLE t3 (id INTEGER);"),
        };
        let report = run_migrations_with(&conn, &registry).expect("retry");
        assert_eq!(report.to_version, 3);
    }

    /// 同一迁移 SQL 批内「前半成功 + 后半失败」也必须整体回滚（批级原子性）。
    #[test]
    fn failed_migration_rolls_back_partial_batch_sql() {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = open_conn(dir.path());
        let registry = vec![
            Migration {
                version: 1,
                description: "v1 ledger",
                step: MigrationStep::Sql(V1_LEDGER_SQL),
            },
            Migration {
                version: 2,
                description: "v2 partial garbage",
                // 第一条合法，第二条是纯语法错误 → execute_batch 整批失败。
                step: MigrationStep::Sql("CREATE TABLE t2b (id INTEGER); CREATE TABLE ("),
            },
        ];

        assert!(run_migrations_with(&conn, &registry).is_err());
        // v1 已成功提交，只有 v2 被回滚 → 版本停在 1。
        assert_eq!(
            current_version(&conn).expect("version"),
            1,
            "仅失败的那条不得前进"
        );
        let t2b: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='t2b'",
                [],
                |r| r.get(0),
            )
            .expect("probe t2b");
        assert_eq!(t2b, 0, "批内前半部分建的表也必须回滚");
        // 内置 v1（与已应用的 v1 幂等）依然可正常跑通。
        assert!(run_migrations(&conn).is_ok());
    }

    /// Custom 闭包迁移：在同一事务内执行程序逻辑并落账本。
    #[test]
    fn custom_closure_migration_applies() {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = open_conn(dir.path());
        let registry = vec![
            Migration {
                version: 1,
                description: "v1 ledger",
                step: MigrationStep::Sql(V1_LEDGER_SQL),
            },
            Migration {
                version: 2,
                description: "v2 custom seed",
                step: MigrationStep::Custom(|tx_conn| {
                    tx_conn
                        .execute_batch(
                            "CREATE TABLE custom_t (v TEXT); INSERT INTO custom_t VALUES ('ok');",
                        )
                        .map_err(map_sqlite)?;
                    Ok(())
                }),
            },
        ];

        let report = run_migrations_with(&conn, &registry).expect("run");
        assert_eq!(report.to_version, 2);
        let v: String = conn
            .query_row("SELECT v FROM custom_t", [], |r| r.get(0))
            .expect("custom row");
        assert_eq!(v, "ok");
        // 闭包迁移同样幂等：重跑 no-op、数据不重复。
        assert!(run_migrations_with(&conn, &registry)
            .expect("rerun")
            .is_noop());
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM custom_t", [], |r| r.get(0))
            .expect("count");
        assert_eq!(rows, 1);
    }

    /// 注册表校验：版本倒退 / 重复 / 为 0 → ConfigError，且不执行任何迁移。
    #[test]
    fn registry_with_bad_versions_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = open_conn(dir.path());
        let bad = vec![
            Migration {
                version: 2,
                description: "first",
                step: MigrationStep::Sql(V1_LEDGER_SQL),
            },
            Migration {
                version: 2,
                description: "duplicate",
                step: MigrationStep::Sql("CREATE TABLE dup (id INTEGER);"),
            },
        ];
        let err = run_migrations_with(&conn, &bad).expect_err("must reject");
        assert_eq!(err.error_code(), ERR_CONFIG);
        assert_eq!(
            current_version(&conn).expect("version"),
            0,
            "不得执行任何迁移"
        );

        let zero = vec![Migration {
            version: 0,
            description: "zero",
            step: MigrationStep::Sql(V1_LEDGER_SQL),
        }];
        let err0 = run_migrations_with(&conn, &zero).expect_err("version 0 must reject");
        assert_eq!(err0.error_code(), ERR_CONFIG);
    }

    /// Forward-only：库版本高于内置注册表 → 拒绝（旧程序开新库）。
    #[test]
    fn database_newer_than_registry_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = open_conn(dir.path());
        conn.pragma_update(None, "user_version", 99_i64)
            .expect("set user_version");

        let err = run_migrations(&conn).expect_err("newer db must be rejected");
        assert_eq!(err.error_code(), ERR_STORAGE);
        assert!(
            err.to_string().contains("forward-only"),
            "错误须说明 forward-only: {err}"
        );
        assert_eq!(
            current_version(&conn).expect("version"),
            99,
            "版本不得被改动"
        );
    }

    /// integrity_check 通过路：内置迁移后全项一致（账本恰为 1..=user_version）。
    #[test]
    fn integrity_check_passes_after_migration() {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = open_conn(dir.path());
        run_migrations(&conn).expect("run");

        let report = integrity_check(&conn).expect("integrity ok");
        assert_eq!(report.user_version, 1);
        assert_eq!(report.registry_latest, builtin_latest_version());
        assert_eq!(report.ledger_versions, vec![1]);
    }

    /// integrity_check 失败路 1：账本行与 user_version 不一致。
    #[test]
    fn integrity_check_fails_on_ledger_mismatch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = open_conn(dir.path());
        run_migrations_with(&conn, &registry_two()).expect("run");

        // 篡改账本：删掉 v2 → 账本 {1} 与 user_version=2 不一致。
        conn.execute("DELETE FROM schema_migrations WHERE version = 2", [])
            .expect("delete ledger row");
        let err = integrity_check(&conn).expect_err("ledger mismatch must fail");
        assert_eq!(err.error_code(), ERR_STORAGE);
    }

    /// integrity_check 失败路 2：user_version 超前于注册表（且空库场景通过）。
    #[test]
    fn integrity_check_fails_when_user_version_exceeds_registry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = open_conn(dir.path());
        // 空库（user_version=0）：应通过（无账本是正常的）。
        let fresh = integrity_check(&conn).expect("fresh db ok");
        assert_eq!(fresh.user_version, 0);
        assert!(fresh.ledger_versions.is_empty());

        conn.pragma_update(None, "user_version", 50_i64)
            .expect("set");
        let err = integrity_check(&conn).expect_err("too-new version must fail");
        assert_eq!(err.error_code(), ERR_STORAGE);
        assert!(err.to_string().contains("newer binary"), "err: {err}");
    }

    /// LoadOutcome 三态语义 + 判别辅助方法。
    #[test]
    fn load_outcome_three_states() {
        let ok: LoadOutcome<u32> = LoadOutcome::Ok(7);
        assert!(!ok.is_safe_mode());
        assert!(!ok.is_recovered());

        let recovered: LoadOutcome<u32> = LoadOutcome::Recovered {
            config: 8,
            warnings: vec!["missing field `a` defaulted".to_string()],
        };
        assert!(recovered.is_recovered());
        assert!(!recovered.is_safe_mode());
        match &recovered {
            LoadOutcome::Recovered { config, warnings } => {
                assert_eq!(*config, 8);
                assert_eq!(warnings.len(), 1);
            }
            _ => panic!("test-only: unreachable"),
        }

        let safe: LoadOutcome<u32> = LoadOutcome::SafeMode {
            reason: "toml parse failed".to_string(),
        };
        assert!(safe.is_safe_mode());
        match &safe {
            LoadOutcome::SafeMode { reason } => assert_eq!(reason, "toml parse failed"),
            _ => panic!("test-only: unreachable"),
        }
        // Debug 不 panic。
        assert!(format!("{ok:?} {recovered:?} {safe:?}").contains("SafeMode"));
    }

    /// 原子备份：内容完整、原文件不动、临时文件不残留。
    #[test]
    fn backup_before_rewrite_is_atomic_and_preserves_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(&cfg_path, b"version = 1\n[gw]\nid = \"a\"\n").expect("seed");

        let backup = backup_before_rewrite(&cfg_path).expect("backup");
        assert!(backup.is_file(), "备份文件必须存在");
        let name = backup.file_name().and_then(|n| n.to_str()).expect("name");
        assert!(
            is_backup_file_name(name, "config.toml") && name.ends_with(".bak"),
            "备份命名: {name}"
        );
        let content = std::fs::read(&backup).expect("read backup");
        assert_eq!(
            content, b"version = 1\n[gw]\nid = \"a\"\n",
            "备份内容必须与原文逐字节一致"
        );
        // 原文件未被改动。
        let original = std::fs::read(&cfg_path).expect("read original");
        assert_eq!(original, b"version = 1\n[gw]\nid = \"a\"\n");
        // 目录里不残留 .tmp 半成品。
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "不得残留临时文件: {leftovers:?}");
        // 连续备份两次 → 两个不同备份名（不互相覆盖）。
        let backup2 = backup_before_rewrite(&cfg_path).expect("backup 2");
        assert_ne!(backup, backup2, "第二次备份不得覆盖第一次");
    }

    /// 备份目标不存在 → 返回错误（不 panic、不创建任何文件）。
    #[test]
    fn backup_before_rewrite_missing_file_is_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("no-such-config.toml");
        let err = backup_before_rewrite(&missing).expect_err("must error");
        assert_eq!(err.error_code(), ERR_CONFIG);
        assert!(
            err.to_string().contains("not an existing regular file"),
            "err: {err}"
        );
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .collect();
        assert!(entries.is_empty(), "失败路径不得创建任何文件");
    }

    /// QA 回归（真机缺陷实证）：相对裸文件名（`config.toml`）的 `parent()` 是
    /// 空串，`read_dir("")` 失败导致 retention/计数双双静默 no-op——目录解析
    /// 必须回退 `"."`（cwd），否则真机清理永远不生效而单测（绝对路径）全绿。
    #[test]
    fn backup_search_dir_falls_back_to_cwd_for_bare_relative_path() {
        // 裸文件名 → 空目录分量 → 回退 "."。
        let dir = backup_search_dir(Path::new("config.toml"));
        assert_eq!(
            dir,
            PathBuf::from("."),
            "bare relative name must resolve to cwd"
        );
        // 带目录分量的相对路径 → parent 原样保留。
        assert_eq!(
            backup_search_dir(Path::new("conf/config.toml")),
            PathBuf::from("conf")
        );
        // 绝对路径 → parent 原样保留（单测既有路径）。
        let absolute = std::env::temp_dir().join("some-config.toml");
        assert_eq!(
            backup_search_dir(&absolute),
            absolute.parent().expect("parent")
        );
        // 显式 `./config.toml` → "."（本就可用，不受修复影响）。
        assert_eq!(
            backup_search_dir(Path::new("./config.toml")),
            PathBuf::from(".")
        );
    }

    /// 备份命名：新格式（可读时间）与旧格式（epoch）都能识别；排序键统一到
    /// epoch 毫秒（升序 = 时间序）；用户自建文件与临时文件绝不误判。
    #[test]
    fn backup_naming_recognizes_new_and_legacy_and_orders_by_time() {
        let cfg = "config.toml";
        // 新格式：识别 + 排序友好（字典序 = 时间序）。
        let mut new_names = vec![
            "config.toml.20260927-011231-002.bak",
            "config.toml.20260927-011231-000.bak",
            "config.toml.20260927-011231-001.bak",
            "config.toml.20260928-000000-000.bak",
        ];
        for name in &new_names {
            assert!(is_backup_file_name(name, cfg), "应识别新格式: {name}");
        }
        new_names.sort();
        assert_eq!(
            new_names,
            vec![
                "config.toml.20260927-011231-000.bak",
                "config.toml.20260927-011231-001.bak",
                "config.toml.20260927-011231-002.bak",
                "config.toml.20260928-000000-000.bak",
            ],
            "字典序 = 时间序"
        );
        // 新格式排序键：同秒内序号单调，跨秒更大。
        let k0 = backup_sort_key("config.toml.20260927-011231-000.bak", cfg);
        let k1 = backup_sort_key("config.toml.20260927-011231-001.bak", cfg);
        let k_next_day = backup_sort_key("config.toml.20260928-000000-000.bak", cfg);
        assert!(k0 < k1 && k1 < k_next_day, "排序键时间升序");

        // 旧格式全部命中，且与新格式可比较（同一 epoch 毫秒时间轴）。
        for name in [
            "config.toml.bak-1790441651",
            "config.toml.bak-1790441651-2",
            "config.toml.bak-manual-1790441651123",
            "config.toml.bak-periodic-1790441651123",
        ] {
            assert!(is_backup_file_name(name, cfg), "应识别旧格式: {name}");
        }
        // UTC+8 转换正确性：epoch 1790441651 = 2026-09-27 00:54:11 (UTC+8)。
        assert_eq!(format_utc8_token(1790441651), "20260927-005411");
        assert_eq!(
            parse_readable_token("20260927-005411"),
            Some(1790441651 * 1000),
            "新格式 token 必须按 UTC+8 反推回同一 epoch"
        );
        // 同一时刻：旧秒格式 ↔ 新格式排序键**精确相等**（统一 epoch 毫秒时间轴）。
        let legacy = backup_sort_key("config.toml.bak-1790441651", cfg);
        let readable = backup_sort_key("config.toml.20260927-005411-000.bak", cfg);
        assert_eq!(
            legacy, readable,
            "同一时刻的新旧命名必须落在同一排序键: {legacy} vs {readable}"
        );

        // 不误判：用户自建文件 / 临时文件 / 其它配置 / 结构不合法。
        for name in [
            "config.toml.mybak-keepme",
            "config.toml.20260927-011231-000.bak.tmp-1234",
            "other.toml.20260927-011231-000.bak",
            "config.toml.2026-0927-011231-000.bak",
            "config.toml.bak-",
        ] {
            assert!(!is_backup_file_name(name, cfg), "不应识别: {name}");
        }
    }

    /// 新命名 helper：同秒内多次备份必须唯一，且文件名内嵌 UTC+8 可读时刻。
    #[test]
    fn next_backup_path_is_unique_and_readable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = "config.toml";
        let first = next_backup_path(dir.path(), cfg);
        let first_name = first
            .file_name()
            .and_then(|n| n.to_str())
            .expect("name")
            .to_string();
        assert!(
            is_backup_file_name(&first_name, cfg) && first_name.ends_with(".bak"),
            "新命名可识别: {first_name}"
        );
        assert!(
            first_name.ends_with("-000.bak"),
            "首份同秒备份序号应为 000: {first_name}"
        );
        // 落第一份后，第二份必须唯一（同秒内换序号；跨秒则换时间戳）。
        std::fs::write(&first, b"a").expect("write first");
        let second = next_backup_path(dir.path(), cfg);
        assert_ne!(first, second, "第二次备份必须唯一、不覆盖第一份");
        let second_name = second
            .file_name()
            .and_then(|n| n.to_str())
            .expect("name")
            .to_string();
        assert!(
            is_backup_file_name(&second_name, cfg) && second_name.ends_with(".bak"),
            "第二份新命名可识别: {second_name}"
        );
    }

    /// 配置升级链：v1→v2→v3 顺序执行；未知起点版本报错（不走改动）。
    #[test]
    fn config_upgrade_chain_applies_in_order_and_rejects_unknown_version() {
        let step1 = FnUpgradeStep::new(1, 2, |raw| {
            Ok(format!(
                "{raw}\n# upgraded to v2\n[added_v2]\nflag = true\n"
            ))
        })
        .expect("step1");
        let step2 = FnUpgradeStep::new(2, 3, |raw| {
            Ok(format!("{raw}\n[added_v3]\nnote = \"ok\"\n"))
        })
        .expect("step2");

        let mut chain = ConfigMigrationChain::new();
        chain.push(Box::new(step1)).expect("push 1");
        chain.push(Box::new(step2)).expect("push 2");
        assert_eq!(chain.latest_version(), Some(3));

        // 从 v1 走全链。
        let cfg = MigrateConfig::new(1, "version = 1\n");
        let outcome = cfg.upgrade_with(&chain).expect("upgrade");
        assert_eq!(outcome.to_version, 3);
        assert_eq!(outcome.applied_steps, vec![1, 2]);
        assert!(outcome.toml.starts_with("version = 1\n"));
        assert!(outcome.toml.contains("[added_v2]"));
        assert!(outcome.toml.contains("[added_v3]"));

        // 从中间版本（v2）走：只执行 v2→v3 一步。
        let mid = MigrateConfig::new(2, "version = 2\n");
        let outcome2 = mid.upgrade_with(&chain).expect("upgrade from 2");
        assert_eq!(outcome2.to_version, 3);
        assert_eq!(outcome2.applied_steps, vec![2]);
        assert!(!outcome2.toml.contains("[added_v2]"));

        // 未知起点（v5，超前于链顶 v3）→ ConfigError，且失败不会产出半成品文本。
        let unknown = MigrateConfig::new(5, "version = 5\n");
        let err = unknown.upgrade_with(&chain).expect_err("unknown from");
        assert_eq!(err.error_code(), ERR_CONFIG);
        assert!(
            err.to_string()
                .contains("no config upgrade path from version 5"),
            "err: {err}"
        );

        // 已站在链顶（v3）→ no-op 成功，文本原样返回。
        let at_top = MigrateConfig::new(3, "version = 3\n");
        let outcome3 = at_top.upgrade_with(&chain).expect("no-op at top");
        assert_eq!(outcome3.to_version, 3);
        assert!(outcome3.applied_steps.is_empty());
        assert_eq!(outcome3.toml, "version = 3\n");
    }
}
