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

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

/// 新鲜窗口下限（毫秒）：慢周期设备（如 60s）也能在合理时间内被判定。
const MIN_FRESH_WINDOW_MS: u64 = 5_000;
/// 新鲜窗口 = 该设备最小采集周期的倍数。
const FRESH_WINDOW_PERIOD_FACTOR: u64 = 3;
/// 判定 `error` 所需的连续失败次数。
pub const ERROR_FAIL_STREAK: u64 = 3;

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
#[derive(Debug, Default)]
pub struct DeviceHealthRegistry {
    inner: Mutex<HashMap<String, DeviceHealth>>,
}

impl DeviceHealthRegistry {
    /// 创建空注册表。
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
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
}
