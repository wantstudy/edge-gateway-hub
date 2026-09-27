//! 设备真实运行健康度注册表（需求 1 根因修复）。
//!
//! # 背景
//! 修复前 `GET /api/devices` 的行**没有**运行状态字段，前端只能默认显示离线——
//! 「新增设备后列表一直显示离线」的根因即在此。本模块由**真实采集路径**
//! （[`crate::dataplane::NorthDataPlane::poll`] 包裹真实南向读
//! [`crate::southbound::DevicePollHandler`]）逐拍写入每设备成功 / 失败事实，
//! 管理面据此计算三态健康度——**绝不伪造**。
//!
//! # 关键区分：「从未采过」vs「采集失败」
//! - 从未成功（`last_success_ms == None`）→ `offline`：设备**从未采过**；
//! - 历史上成功过，但已超出新鲜窗口且连续失败 ≥ 3 → `error`：设备**采集失败**；
//! - 距最近一次成功 ≤ 新鲜窗口 → `online`。
//!
//! # 判定语义（写死在本文件，注释即契约）
//! ```text
//! window = max(3 × device_min_frequency_ms, 5000ms)   // 该设备最小采集周期
//! if last_success_ms == None                          -> offline   (从未采过)
//! else if now - last_success_ms <= window             -> online
//! else if consecutive_failures >= 3                   -> error     (历史成功过)
//! else                                                -> offline   (陈旧但未达 error)
//! ```
//! 「在窗口内」优先于「连续失败」：窗口本身按采集周期自校准，短暂抖动不误报。
//!
//! # 设备心跳（主动探活）—— 与轮询三态并列的独立维度
//! 轮询三态（`status_for`）是**被动推导**（采集成败）；设备心跳是**主动探活**
//! （设备经 `POST /api/devices/:id/heartbeat` 主动上报）。二者**共存、互不覆盖**：
//! 本模块只负责心跳的持久化状态载体与新鲜度判定（`beat_status`），**绝不改写**
//! 由 `status_for` 计算的三态 `status`。呈现层若需合并，由 `mgmt::mod.rs`（wave2）
//! 在现有设备行**追加** `last_beat_at` / `beat_status` 字段完成。
//!
//! ## 持久化
//! 心跳时间必须重启后仍可见：独立库文件 `device_heartbeat.db`（单写者 = 本模块
//! 的 [`DeviceHeartbeatStore`]，遵循项目「分库、各库单写者」风格，不跨库写）。
//! 进程内叠加层（`beat_cache`）加速读；未命中回落到库。生产环境经
//! `DeviceHealthRegistry::new()` 的**惰性打开**（首次上报 / 查询时按
//! `IOT_DAQ_DATA_DIR` 解析路径）挂载；测试用 [`DeviceHealthRegistry::open_heartbeat_db`]
//! 显式指向临时库，直接证明「重开库后心跳仍在」。

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{DaemonError, DaemonResult};
use rusqlite::{params, Connection};
use tracing::warn;

/// 新鲜窗口下限（毫秒）：慢周期设备（如 60s）也能在合理时间内被判定。
const MIN_FRESH_WINDOW_MS: u64 = 5_000;
/// 新鲜窗口 = 该设备最小采集周期的倍数。
const FRESH_WINDOW_PERIOD_FACTOR: u64 = 3;
/// 判定 `error` 所需的连续失败次数。
pub const ERROR_FAIL_STREAK: u64 = 3;

// ---- 设备心跳（主动探活）----

/// 心跳新鲜窗口（毫秒）：超过此窗口未收到心跳 → `BeatStatus::Stale`。
pub const BEAT_FRESH_WINDOW_MS: u64 = 60_000;
/// 心跳时刻允许的未来偏移上限（毫秒）：用于拒绝明显时钟超前的伪上报。
pub const MAX_FUTURE_SKEW_MS: u64 = 60_000;
/// 允许的最早 epoch 毫秒（2015-01-01）：过滤明显非法的「0 / 远古」值。
pub const MIN_PLAUSIBLE_BEAT_MS: u64 = 1_420_070_400_000;

/// 设备心跳新鲜度（与轮询三态并列的独立维度）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeatStatus {
    /// 在新鲜窗口内收到过心跳（设备主动探活存活）。
    Online,
    /// 曾上报过，但已超过新鲜窗口（探活超时）。
    Stale,
    /// 从未上报过心跳（无探活数据）。
    Unknown,
}

impl BeatStatus {
    /// 对外字面量（前端契约）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            BeatStatus::Online => "online",
            BeatStatus::Stale => "stale",
            BeatStatus::Unknown => "unknown",
        }
    }
}

/// 由最近心跳时刻 + 当前时刻判定心跳新鲜度（纯函数，见模块注释）。
#[must_use]
pub fn beat_status_for(last_beat_ms: Option<u64>, now_ms: u64) -> BeatStatus {
    match last_beat_ms {
        None => BeatStatus::Unknown,
        Some(ts) if now_ms.saturating_sub(ts) <= BEAT_FRESH_WINDOW_MS => BeatStatus::Online,
        Some(_) => BeatStatus::Stale,
    }
}

/// rusqlite 错误 → `StorageError`（4000），绝不 panic。
fn map_sqlite(err: rusqlite::Error) -> DaemonError {
    DaemonError::StorageError(format!("heartbeat db: {err}"))
}

/// 设备心跳持久载体（独立库文件 `device_heartbeat.db`，单写者 = 本结构）。
///
/// 不跨库写：与 `audit.db` / `telemetry.db` / `queue.db` 物理隔离，各自单写者。
#[derive(Debug)]
pub struct DeviceHeartbeatStore {
    /// 写连接（单写者；`Connection` 非 `Sync`，由 `Mutex` 串行化）。
    conn: Mutex<Connection>,
}

impl DeviceHeartbeatStore {
    /// 库文件名（固定；与 audit.db 等同级单写者库）。
    pub const DB_FILE_NAME: &'static str = "device_heartbeat.db";

    /// 打开（或创建）心跳库并建立 `device_beat` 表（幂等）。
    ///
    /// # Errors
    /// 目录不可建 / 连接失败 / 建表失败 → [`DaemonError::StorageError`]。
    pub fn open(path: &Path) -> DaemonResult<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(path).map_err(map_sqlite)?;
        // 并发连接（如并行测试 / 多任务上报）串行等待而非直接 BUSY 失败。
        conn.execute_batch(
            "PRAGMA busy_timeout = 5000;
             CREATE TABLE IF NOT EXISTS device_beat (
                 device_id   TEXT PRIMARY KEY NOT NULL,
                 beat_at_ms  INTEGER NOT NULL
             )",
        )
        .map_err(map_sqlite)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// 记录一次心跳：单调取大（乱序上报不回退），`INSERT ... ON CONFLICT DO UPDATE`。
    ///
    /// # Errors
    /// 写入失败（IO / 锁 / 约束）→ [`DaemonError::StorageError`]。
    pub fn record_beat(&self, device_id: &str, now_ms: u64) -> DaemonResult<()> {
        let ts = i64::try_from(now_ms)
            .map_err(|_| DaemonError::StorageError(format!("beat timestamp overflow: {now_ms}")))?;
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        conn.execute(
            "INSERT INTO device_beat (device_id, beat_at_ms) VALUES (?1, ?2)
             ON CONFLICT(device_id) DO UPDATE SET beat_at_ms = MAX(beat_at_ms, ?2)",
            params![device_id, ts],
        )
        .map_err(map_sqlite)?;
        Ok(())
    }

    /// 读取某设备最近一次心跳（毫秒）。
    ///
    /// # Errors
    /// 查询失败 → [`DaemonError::StorageError`]。
    pub fn last_beat_ms(&self, device_id: &str) -> DaemonResult<Option<u64>> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let result: Result<Option<i64>, rusqlite::Error> = conn.query_row(
            "SELECT beat_at_ms FROM device_beat WHERE device_id = ?1",
            params![device_id],
            |row| row.get::<_, Option<i64>>(0),
        );
        match result {
            Ok(value) => Ok(value.map(|v| v.max(0) as u64)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(err) => Err(map_sqlite(err)),
        }
    }

    /// 已登记心跳设备数（诊断 / 测试用）。
    #[must_use]
    pub fn len(&self) -> usize {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        conn.query_row("SELECT COUNT(*) FROM device_beat", [], |row| row.get::<_, i64>(0))
            .map(|n| n.max(0) as usize)
            .unwrap_or(0)
    }

    /// 是否无任何心跳记录。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 单个设备的运行健康度（原子事实的只读快照）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeviceHealth {
    /// 累计轮询尝试次数（成功 + 失败）。
    pub polls: u64,
    /// 累计轮询失败次数。
    pub errors: u64,
    /// 最近一次成功之后的连续失败次数（成功即清零）。
    pub consecutive_failures: u64,
    /// 最近一次成功轮询的 UTC 毫秒时间戳（`None` = 从未成功）。
    pub last_success_ms: Option<u64>,
    /// 最近一次失败轮询的 UTC 毫秒时间戳（`None` = 从未失败）。
    pub last_fail_ms: Option<u64>,
}

/// 设备三态健康度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceStatus {
    /// 距最近一次成功轮询 ≤ 新鲜窗口。
    Online,
    /// 从未成功，或已超出窗口但未达 error 条件。
    Offline,
    /// 历史成功过，且已超出窗口并连续失败 ≥ [`ERROR_FAIL_STREAK`]。
    Error,
}

impl DeviceStatus {
    /// 对外字面量（前端契约）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            DeviceStatus::Online => "online",
            DeviceStatus::Offline => "offline",
            DeviceStatus::Error => "error",
        }
    }
}

/// 当前 UTC 毫秒时间戳（系统时钟回拨 / 越界时回退 0，绝不 panic）。
#[must_use]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// 设备健康度注册表（跨任务共享；内部 `Mutex`，毒锁恢复不 panic）。
///
/// 同时持有心跳**持久**后端（[`DeviceHeartbeatStore`]，可空）：`new()` 为纯进程内
/// 态（零文件副作用，保持既有行为）；生产经惰性打开挂载默认库，测试经
/// [`DeviceHealthRegistry::open_heartbeat_db`] 显式指向临时库。
#[derive(Debug, Default)]
pub struct DeviceHealthRegistry {
    /// 轮询成败三态事实（与心跳维度互不覆盖）。
    inner: Mutex<HashMap<String, DeviceHealth>>,
    /// 心跳时刻进程内叠加层（写时同步更新；读未命中时回落到库）。
    beat_cache: Mutex<HashMap<String, u64>>,
    /// 心跳持久后端（`None` = 仅进程内，重启即失）。
    beat_store: Mutex<Option<Arc<DeviceHeartbeatStore>>>,
    /// 默认库惰性打开是否曾失败（失败则不再反复重试，避免每次上报都打日志）。
    beat_open_failed: Mutex<bool>,
}

impl DeviceHealthRegistry {
    /// 创建空注册表（纯进程内；无文件副作用，保持既有行为）。
    ///
    /// 生产环境的持久后端由首次上报 / 查询时**惰性打开**默认库挂载
    ///（见 [`Self::ensure_store`]）；失败则降级为内存态（端点据实 503）。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 打开指定库文件并构造注册表（测试用；直接证明「重开库后心跳仍在」）。
    ///
    /// # Errors
    /// 库打开 / 建表失败 → [`DaemonError::StorageError`]。
    pub fn open_heartbeat_db(path: &Path) -> DaemonResult<Self> {
        let store = DeviceHeartbeatStore::open(path)?;
        Ok(Self {
            inner: Mutex::new(HashMap::new()),
            beat_cache: Mutex::new(HashMap::new()),
            beat_store: Mutex::new(Some(Arc::new(store))),
            beat_open_failed: Mutex::new(false),
        })
    }

    /// 以已打开的持久后端构造注册表（测试 / 装配用）。
    #[must_use]
    pub fn with_heartbeat_store(store: Arc<DeviceHeartbeatStore>) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            beat_cache: Mutex::new(HashMap::new()),
            beat_store: Mutex::new(Some(store)),
            beat_open_failed: Mutex::new(false),
        }
    }

    /// 持久后端是否已挂载（端点据以区分「已持久」与「仅内存」）。
    #[must_use]
    pub fn beat_store_mounted(&self) -> bool {
        self.beat_store
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }

    /// 惰性打开默认库（`IOT_DAQ_DATA_DIR` env → `<dir>/device_heartbeat.db`，否则
    /// `./device_heartbeat.db`）；best-effort：失败只记 warn，不阻断、不 panic。
    fn ensure_store(&self) {
        {
            let failed = self
                .beat_open_failed
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if *failed {
                return;
            }
            let guard = self
                .beat_store
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if guard.is_some() {
                return;
            }
        }
        let path = match default_beat_db_path() {
            Ok(p) => p,
            Err(err) => {
                warn!(error = %err, "heartbeat: cannot resolve default store path");
                *self
                    .beat_open_failed
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = true;
                return;
            }
        };
        match DeviceHeartbeatStore::open(&path) {
            Ok(store) => {
                *self
                    .beat_store
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(store));
            }
            Err(err) => {
                warn!(
                    error = %err,
                    path = %path.display(),
                    "heartbeat: persistent store unavailable; beats kept in-memory only this run"
                );
                *self
                    .beat_open_failed
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = true;
            }
        }
    }

    /// 取锁并在中毒时取回内部数据（**绝不 panic**）。
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, DeviceHealth>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 记录一次**成功**轮询：累计轮询 +1、连续失败清零、刷新最近成功时刻。
    pub fn record_success(&self, device_id: &str, now_ms: u64) {
        let mut map = self.lock();
        let entry = map.entry(device_id.to_string()).or_default();
        entry.polls = entry.polls.saturating_add(1);
        entry.consecutive_failures = 0;
        entry.last_success_ms = Some(match entry.last_success_ms {
            Some(prev) => prev.max(now_ms),
            None => now_ms,
        });
    }

    /// 记录一次**失败**轮询：累计轮询与失败各 +1、连续失败 +1、刷新最近失败时刻。
    pub fn record_failure(&self, device_id: &str, now_ms: u64) {
        let mut map = self.lock();
        let entry = map.entry(device_id.to_string()).or_default();
        entry.polls = entry.polls.saturating_add(1);
        entry.errors = entry.errors.saturating_add(1);
        entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
        entry.last_fail_ms = Some(match entry.last_fail_ms {
            Some(prev) => prev.max(now_ms),
            None => now_ms,
        });
    }

    /// 取某设备健康度快照（`None` = 从未轮询过该设备）。
    #[must_use]
    pub fn snapshot(&self, device_id: &str) -> Option<DeviceHealth> {
        self.lock().get(device_id).copied()
    }

    /// 已登记设备数（诊断 / 测试用）。
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// 是否无任何设备记录。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// 记录一次设备心跳（主动探活）。
    ///
    /// 持久化（若后端已挂载 / 惰性打开成功），并同步进程内叠加层。返回**实际落库/
    /// 叠加层采用的时刻**（单调取大，乱序上报不回退）。
    ///
    /// # Errors
    /// 持久后端已挂载但写入失败（IO / 锁）→ [`DaemonError::StorageError`]
    ///（未挂载时仅写内存态并返回当前值，无错误）。
    pub fn record_beat(&self, device_id: &str, now_ms: u64) -> DaemonResult<u64> {
        self.ensure_store();
        let stored = {
            let guard = self
                .beat_store
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            match guard.as_ref() {
                Some(store) => {
                    store.record_beat(device_id, now_ms)?;
                    now_ms
                }
                None => now_ms,
            }
        };
        let mut cache = self
            .beat_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        cache
            .entry(device_id.to_string())
            .and_modify(|v| *v = (*v).max(stored))
            .or_insert(stored);
        Ok(stored)
    }

    /// 读取某设备最近心跳（毫秒）：进程内叠加层命中优先，未命中回落持久库。
    ///
    /// # Errors
    /// 持久库查询失败 → [`DaemonError::StorageError`]。
    pub fn last_beat_ms(&self, device_id: &str) -> DaemonResult<Option<u64>> {
        {
            let cache = self
                .beat_cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(v) = cache.get(device_id) {
                return Ok(Some(*v));
            }
        }
        self.ensure_store();
        let guard = self
            .beat_store
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match guard.as_ref() {
            Some(store) => store.last_beat_ms(device_id),
            None => Ok(None),
        }
    }

    /// 读某设备心跳新鲜度（与轮询三态并列，互不覆盖）。
    ///
    /// # Errors
    /// 持久库查询失败 → [`DaemonError::StorageError`]。
    pub fn beat_status(&self, device_id: &str, now_ms: u64) -> DaemonResult<BeatStatus> {
        let last = self.last_beat_ms(device_id)?;
        Ok(beat_status_for(last, now_ms))
    }
}

/// 解析默认心跳库路径（生产惰性打开用）。
///
/// `IOT_DAQ_DATA_DIR` 已设 → `<dir>/device_heartbeat.db`；否则 `./device_heartbeat.db`
///（与 audit.db 的「配置同目录」旧口径同源，沿用既有持久卷风格）。
fn default_beat_db_path() -> DaemonResult<std::path::PathBuf> {
    let dir = std::env::var("IOT_DAQ_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    if !dir.as_os_str().is_empty() {
        std::fs::create_dir_all(&dir)?;
    }
    Ok(dir.join(DeviceHeartbeatStore::DB_FILE_NAME))
}

/// 计算新鲜窗口（毫秒）：`max(3 × min_freq_ms, 5000)`。
///
/// `min_freq_ms == 0`（无点位设备）时回退到 [`MIN_FRESH_WINDOW_MS`]。
#[must_use]
pub fn fresh_window_ms(min_freq_ms: u64) -> u64 {
    min_freq_ms
        .saturating_mul(FRESH_WINDOW_PERIOD_FACTOR)
        .max(MIN_FRESH_WINDOW_MS)
}

/// 由健康度 + 该设备最小采集周期 + 当前时刻判定三态（纯函数，见模块注释契约）。
#[must_use]
pub fn status_for(health: Option<&DeviceHealth>, min_freq_ms: u64, now_ms: u64) -> DeviceStatus {
    let Some(health) = health else {
        return DeviceStatus::Offline; // 从未轮询 = 从未采过。
    };
    let Some(last_success) = health.last_success_ms else {
        return DeviceStatus::Offline; // 从未成功 = 从未采过。
    };
    if now_ms.saturating_sub(last_success) <= fresh_window_ms(min_freq_ms) {
        return DeviceStatus::Online;
    }
    if health.consecutive_failures >= ERROR_FAIL_STREAK {
        return DeviceStatus::Error;
    }
    DeviceStatus::Offline
}

/// 成功率（0—100，保留 1 位小数）：`(polls - errors) / polls`。
///
/// 从未轮询（`polls == 0`）→ `0.0`（无数据，不伪装 100%）。
#[must_use]
pub fn success_rate(health: Option<&DeviceHealth>) -> f64 {
    let Some(health) = health else {
        return 0.0;
    };
    if health.polls == 0 {
        return 0.0;
    }
    let ok = health.polls.saturating_sub(health.errors);
    let rate = (ok as f64) / (health.polls as f64) * 100.0;
    (rate * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 从未采过（无记录 / 无成功）→ offline，且与「采集失败」可区分。
    #[test]
    fn never_sampled_is_offline_not_error() {
        // 无任何记录。
        assert_eq!(status_for(None, 1000, 1_000_000), DeviceStatus::Offline);
        // 只失败过、从未成功 → 仍 offline（不是 error：error 要求历史成功过）。
        let only_failed = DeviceHealth {
            polls: 5,
            errors: 5,
            consecutive_failures: 5,
            last_success_ms: None,
            last_fail_ms: Some(999_000),
        };
        assert_eq!(
            status_for(Some(&only_failed), 1000, 1_000_000),
            DeviceStatus::Offline,
            "never-succeeded device must be offline, distinguishable from error"
        );
    }

    /// 成功窗口内 → online；超窗且连续失败 ≥3 → error；超窗但失败不足 → offline。
    #[test]
    fn three_state_judgement() {
        let now = 1_000_000u64;
        let window = fresh_window_ms(1000); // max(3000, 5000) = 5000
        assert_eq!(window, 5_000);

        // 刚刚成功 → online。
        let fresh = DeviceHealth {
            polls: 1,
            errors: 0,
            consecutive_failures: 0,
            last_success_ms: Some(now - 1),
            last_fail_ms: None,
        };
        assert_eq!(status_for(Some(&fresh), 1000, now), DeviceStatus::Online);

        // 边界：恰好在窗口内（差 = window）→ online。
        let boundary = DeviceHealth {
            last_success_ms: Some(now - window),
            ..fresh
        };
        assert_eq!(status_for(Some(&boundary), 1000, now), DeviceStatus::Online);
        // 超出窗口 1ms 且失败不足 3 → offline。
        let stale = DeviceHealth {
            last_success_ms: Some(now - window - 1),
            consecutive_failures: 2,
            ..fresh
        };
        assert_eq!(status_for(Some(&stale), 1000, now), DeviceStatus::Offline);
        // 超出窗口且连续失败 ≥3 → error。
        let erroring = DeviceHealth {
            last_success_ms: Some(now - window - 1),
            consecutive_failures: 3,
            errors: 3,
            polls: 4,
            ..fresh
        };
        assert_eq!(status_for(Some(&erroring), 1000, now), DeviceStatus::Error);
    }

    /// 窗口随采集周期自校准（3× 周期与下限取大）。
    #[test]
    fn window_scales_with_period() {
        assert_eq!(fresh_window_ms(0), 5_000, "no-period falls back to floor");
        assert_eq!(fresh_window_ms(1_000), 5_000, "floor dominates");
        assert_eq!(fresh_window_ms(60_000), 180_000, "3x period dominates");
    }

    /// 注册表：成功清零连续失败、失败累加、成功率按成败比例。
    #[test]
    fn registry_tracks_success_failure_and_rate() {
        let registry = DeviceHealthRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.snapshot("dev-1"), None);

        registry.record_success("dev-1", 100);
        registry.record_failure("dev-1", 200);
        registry.record_failure("dev-1", 300);
        let health = registry.snapshot("dev-1").expect("recorded");
        assert_eq!(health.polls, 3);
        assert_eq!(health.errors, 2);
        assert_eq!(health.consecutive_failures, 2);
        assert_eq!(health.last_success_ms, Some(100));
        assert_eq!(health.last_fail_ms, Some(300));
        assert_eq!(registry.len(), 1);

        // 再次成功 → 连续失败清零，成功率 (4-2)/4 = 50.0。
        registry.record_success("dev-1", 400);
        let health = registry.snapshot("dev-1").expect("recorded");
        assert_eq!(health.consecutive_failures, 0);
        assert_eq!(health.polls, 4);
        assert!((success_rate(Some(&health)) - 50.0).abs() < 1e-9);
        assert_eq!(success_rate(None), 0.0, "no data must not fake 100%");

        // 时间只前进不回退（乱序上报取较大值）。
        registry.record_success("dev-1", 50);
        assert_eq!(
            registry
                .snapshot("dev-1")
                .expect("recorded")
                .last_success_ms,
            Some(400)
        );
    }

    /// 状态字面量与前端契约一致。
    #[test]
    fn status_literals() {
        assert_eq!(DeviceStatus::Online.as_str(), "online");
        assert_eq!(DeviceStatus::Offline.as_str(), "offline");
        assert_eq!(DeviceStatus::Error.as_str(), "error");
    }

    // ---- 设备心跳（主动探活） ----

    /// 上报心跳后 `last_beat_ms` 被更新（进程内叠加层命中）。
    #[test]
    fn heartbeat_record_updates_last_beat_at() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry =
            DeviceHealthRegistry::open_heartbeat_db(&dir.path().join("h.db")).expect("open");
        let ts = 1_700_000_000_123u64;
        let stored = registry.record_beat("dev-01", ts).expect("record");
        assert_eq!(stored, ts);
        assert_eq!(registry.last_beat_ms("dev-01").expect("read"), Some(ts));
        // 进程内叠加层命中，无需回查库即返回。
        assert_eq!(registry.beat_status("dev-01", ts).expect("status"), BeatStatus::Online);
    }

    /// 乱序上报不回退：小值被忽略，大值被采纳。
    #[test]
    fn heartbeat_does_not_go_backwards() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry =
            DeviceHealthRegistry::open_heartbeat_db(&dir.path().join("h.db")).expect("open");
        registry.record_beat("dev-01", 2_000_000_000_000).expect("record");
        registry.record_beat("dev-01", 1_000_000_000_000).expect("record");
        assert_eq!(
            registry.last_beat_ms("dev-01").expect("read"),
            Some(2_000_000_000_000)
        );
    }

    /// 重开库（模拟 daemon 重启）：心跳时间仍可见——直接证明持久化生效。
    #[test]
    fn heartbeat_survives_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("h.db");
        {
            let registry = DeviceHealthRegistry::open_heartbeat_db(&path).expect("open");
            registry
                .record_beat("dev-01", 1_700_000_000_123)
                .expect("record");
            assert_eq!(
                registry.last_beat_ms("dev-01").expect("read"),
                Some(1_700_000_000_123)
            );
        }
        // 丢弃旧实例（释放写连接），以全新实例重开同一库文件。
        let registry = DeviceHealthRegistry::open_heartbeat_db(&path).expect("reopen");
        assert_eq!(
            registry.last_beat_ms("dev-01").expect("read after reopen"),
            Some(1_700_000_000_123),
            "心跳必须跨重启（重开库）可见"
        );
        // 未上报过的设备：无探活数据 → Unknown，且不因库里有别的设备而串味。
        assert_eq!(registry.last_beat_ms("never").expect("read"), None);
        assert_eq!(
            registry.beat_status("never", 1_700_000_000_123).expect("status"),
            BeatStatus::Unknown
        );
    }

    /// 心跳新鲜度判定：在窗内 = online、超窗 = stale、无数据 = unknown。
    #[test]
    fn heartbeat_status_semantics() {
        let now = 1_700_000_000_000u64;
        assert_eq!(beat_status_for(None, now), BeatStatus::Unknown);
        assert_eq!(
            beat_status_for(Some(now - 1), now),
            BeatStatus::Online
        );
        assert_eq!(
            beat_status_for(Some(now - BEAT_FRESH_WINDOW_MS), now),
            BeatStatus::Online,
            "窗口边界（恰好窗口内）应为 online"
        );
        assert_eq!(
            beat_status_for(Some(now - BEAT_FRESH_WINDOW_MS - 1), now),
            BeatStatus::Stale
        );
        assert_eq!(BeatStatus::Online.as_str(), "online");
        assert_eq!(BeatStatus::Stale.as_str(), "stale");
        assert_eq!(BeatStatus::Unknown.as_str(), "unknown");
    }
}
