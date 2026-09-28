//! 吞吐与运行指标自包含采集器（task 141 / BE-METRICS）。
//!
//! # 职责边界
//!
//! 本模块是一个**完全自包含**的进程级采集器，对外只暴露「录入入口」与「快照序列化」
//! 两组 `pub` 函数。它**不**持有任何设备表、驱动句柄或队列句柄的引用——采集点
//! 由 wave2 在既有采集路径里显式调用 [`record_point`] / [`record_batch`] /
//! [`record_driver_result`] / [`record_queue_depth`] 喂入（本任务**不做埋点**）。
//!
//! # 诚实退化红线（与 `sim.rs` 同口径）
//!
//! - 任何未被采样的字段，快照里返回 `"available": false` + 真实 `reason`
//!   （例如 `"no samples yet"`），**绝不**编造随机值 / 常量假数据 / 静默回退 0。
//! - 前端约定：拿到 `available:false` 即渲染真实空态，本模块保证不会骗它。
//!
//! # 大数红线
//!
//! `u64` 计数与纳秒时间戳一律序列化为 **JSON 字符串**，避免 `serde_json` 把
//! `> 2^53` 的整数以 `f64` 兜底时丢精度（见单测
//! `snapshot_no_number_exceeds_2_53`）。快照里唯一的数值类型是吞吐率 `f64`，
//! 其量级在真实采集下远低于 `2^53`。
//!
//! # 线程安全 / 零 panic
//!
//! 全局状态为 `OnceLock<Mutex<MetricsState>>`；持锁期间**不**做任何 IO；
//! 非测试代码禁 `unwrap` / `expect` / `panic!`（锁中毒走 `into_inner` 恢复）。

use crate::offline_queue::{Clock, ManualClock, SystemClock};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

/// 1 秒的纳秒数（避免魔法数）。
const NS_PER_SEC: i64 = 1_000_000_000;
/// 单时间片的默认长度（秒）。
const DEFAULT_SLICE_SECONDS: u64 = 1;
/// 环形缓冲默认保留的时间片数（最近 60 秒）。
const DEFAULT_CAPACITY: usize = 60;
/// `slice_seconds` 允许的硬上限（防 `as i64` 溢出 / 不合理配置）。
const MAX_SLICE_SECONDS: u64 = 1_000_000;

/// 可注入时钟（复用 `offline_queue` 的 `Clock` 契约；测试用 [`ManualClock`] 跳过真 sleep）。
pub trait MetricsClock: Send + Sync {
    /// 当前时间（UNIX 纪元起纳秒）。
    fn now_ns(&self) -> i64;
}

impl MetricsClock for SystemClock {
    fn now_ns(&self) -> i64 {
        Clock::now_ns(self)
    }
}

impl MetricsClock for ManualClock {
    fn now_ns(&self) -> i64 {
        Clock::now_ns(self)
    }
}

/// 单个时间片内的聚合（不存逐点明细，禁无界增长）。
#[derive(Default, Clone)]
struct Slice {
    /// 该片起点（纳秒，已对齐到片边界）。
    start_ns: i64,
    /// 该片内录入的点位数。
    points: u64,
    /// 该片内驱动读取成功次数。
    ok: u64,
    /// 该片内驱动读取失败次数。
    fail: u64,
    /// 该片内按设备归并的点位数（仅在 `record_point_device` 被调用后出现）。
    per_device: HashMap<String, u64>,
}

/// 采集器内部状态（全局单例持有）。
struct MetricsState {
    clock: Arc<dyn MetricsClock>,
    /// 单时间片长度（秒）。
    slice_seconds: u64,
    /// 环形缓冲容量（片数）。
    capacity: usize,
    /// 已完成的时间片（最旧在前）。
    ring: VecDeque<Slice>,
    /// 当前正在累积的开放时间片（`None` = 尚未录入任何样本。
    cur: Option<Slice>,
    /// 进程累计点位数（仅诊断，不进快照数值）。
    total_points: u64,
    /// 进程累计驱动成功数。
    total_ok: u64,
    /// 进程累计驱动失败数。
    total_fail: u64,
    /// 最近一次上报的离线队列深度（`None` = 从未采样）。
    queue_depth: Option<u64>,
    /// 是否收到过按设备维度的录入。
    device_seen: bool,
    /// 驱动连接探针结果（`None` = 从未收到 liveness 信号）。
    driver_liveness: Option<bool>,
}

impl MetricsState {
    fn new() -> Self {
        MetricsState {
            clock: Arc::new(SystemClock::new()),
            slice_seconds: DEFAULT_SLICE_SECONDS,
            capacity: DEFAULT_CAPACITY,
            ring: VecDeque::new(),
            cur: None,
            total_points: 0,
            total_ok: 0,
            total_fail: 0,
            queue_depth: None,
            device_seen: false,
            driver_liveness: None,
        }
    }
}

/// 片边界纳秒长度（已对 `secs==0` 做夹紧与溢出保护）。
fn slice_ns(secs: u64) -> i64 {
    let s = if secs == 0 {
        1
    } else {
        secs.min(MAX_SLICE_SECONDS)
    };
    (s as i64).saturating_mul(NS_PER_SEC)
}

/// 由起点推导其所属片 key。
fn slice_key_of(start_ns: i64, secs: u64) -> i64 {
    start_ns / slice_ns(secs)
}

/// 取 / 滚动当前开放时间片（返回 `None` 仅当时钟返回无法落片的异常值，极少发生；
/// 调用方据此优雅跳过本次录入，绝不 panic）。
fn open_slice(state: &mut MetricsState) -> Option<&mut Slice> {
    let secs = state.slice_seconds.max(1);
    let now = state.clock.now_ns();
    let key = now / slice_ns(secs);
    let roll = match &state.cur {
        Some(s) => slice_key_of(s.start_ns, secs) != key,
        None => true,
    };
    if roll {
        if let Some(old) = state.cur.take() {
            // 超出容量：先淘汰最旧片，再存入刚刚完成的片。
            if state.ring.len() >= state.capacity {
                state.ring.pop_front();
            }
            state.ring.push_back(old);
        }
        state.cur = Some(Slice {
            start_ns: key.saturating_mul(slice_ns(secs)),
            ..Slice::default()
        });
    }
    state.cur.as_mut()
}

/// 全局状态句柄（中毒时 `into_inner` 恢复，不 panic）。
fn lock() -> std::sync::MutexGuard<'static, MetricsState> {
    static METRICS: OnceLock<Mutex<MetricsState>> = OnceLock::new();
    let m = METRICS.get_or_init(|| Mutex::new(MetricsState::new()));
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

// ───────────────────────────── 录入入口 ─────────────────────────────

/// 录入一个点位值。
///
/// 纯累加、无副作用 panic；锁内只做计数，不触 IO。
pub fn record_point() {
    record_n(1);
}

/// 批量录入 `n` 个点位值。
///
/// `n == 0` 视为空操作（忽略，不报错）。
pub fn record_batch(n: u64) {
    record_n(n);
}

/// 实际录入逻辑（点位维度，不含设备信息）。
fn record_n(n: u64) {
    if n == 0 {
        return;
    }
    let mut guard = lock();
    if let Some(slice) = open_slice(&mut guard) {
        slice.points = slice.points.saturating_add(n);
        guard.total_points = guard.total_points.saturating_add(n);
    }
}

/// 录入一次南向驱动读取结果（`ok = true` 成功 / `false` 失败）。
pub fn record_driver_result(ok: bool) {
    let mut guard = lock();
    if let Some(slice) = open_slice(&mut guard) {
        if ok {
            slice.ok = slice.ok.saturating_add(1);
            guard.total_ok = guard.total_ok.saturating_add(1);
        } else {
            slice.fail = slice.fail.saturating_add(1);
            guard.total_fail = guard.total_fail.saturating_add(1);
        }
    }
}

/// 录入离线队列（[`crate::offline_queue::OfflineQueue`]）当前深度。
pub fn record_queue_depth(depth: u64) {
    let mut guard = lock();
    guard.queue_depth = Some(depth);
}

/// 录入一次驱动连接探针结果（`connected = true` 在线 / `false` 掉线）。
///
/// 快照的 `driver_connection` 字段依赖此信号；未调用前该字段诚实返回
/// `"available": false`。
pub fn record_driver_liveness(connected: bool) {
    let mut guard = lock();
    guard.driver_liveness = Some(connected);
}

/// 录入带设备维度的点位值（供「每设备吞吐」聚合）。
///
/// 仅在调用后 `per_device` 字段才会 `available: true`；未调用前诚实返回
/// `"available": false`。
pub fn record_point_device(device_id: &str, n: u64) {
    if n == 0 || device_id.is_empty() {
        return;
    }
    let mut guard = lock();
    if let Some(slice) = open_slice(&mut guard) {
        slice
            .per_device
            .entry(device_id.to_string())
            .and_modify(|c| *c = c.saturating_add(n))
            .or_insert(n);
        guard.device_seen = true;
        guard.total_points = guard.total_points.saturating_add(n);
    }
}

/// 配置采集器（时间片长度与环形容量）。
///
/// 仅接受合理区间：`capacity >= 1`、`1 <= slice_seconds <= MAX_SLICE_SECONDS`；
/// 越界字段被忽略（不报错、不 panic）。容量缩小时会即时裁剪已存片。
pub fn configure(capacity: usize, slice_seconds: u64) {
    let mut guard = lock();
    if capacity >= 1 {
        guard.capacity = capacity;
        while guard.ring.len() > capacity {
            guard.ring.pop_front();
        }
    }
    if (1..=MAX_SLICE_SECONDS).contains(&slice_seconds) {
        guard.slice_seconds = slice_seconds;
    }
}

/// 替换采集器时钟（测试用 [`ManualClock`] 跳过真实等待；生产默认 [`SystemClock`]）。
pub fn set_clock(clock: Arc<dyn MetricsClock>) {
    let mut guard = lock();
    guard.clock = clock;
}

// ───────────────────────────── 快照序列化 ─────────────────────────────

/// 生成当前指标快照（[`serde_json::Value`]）。
///
/// 字段（详见模块文档红线）：
/// - `throughput`：瞬时 + 滑动平均吞吐（points/s）；未采样则 `available:false`；
/// - `per_device`：每设备吞吐；未录入设备维度则 `available:false`；
/// - `queue_depth`：离线队列深度；未采样则 `available:false`；
/// - `success_rate`：采集成功率（成功/(成功+失败)）；未采样则 `available:false`；
/// - `driver_connection`：驱动连接状态；无 liveness 信号则 `available:false`；
/// - `window`：采样窗口说明（片长 / 容量 / 已填片数 / 是否已满 / 起点纳秒）。
///
/// 所有 `u64` 计数与纳秒时间戳均为字符串，杜绝 `> 2^53` 的精度丢失。
#[must_use]
pub fn snapshot() -> Value {
    let guard = lock();
    let state = &*guard;
    let secs = state.slice_seconds.max(1);

    let ring_len = state.ring.len();
    let filled = ring_len + usize::from(state.cur.is_some());
    let ring_full = ring_len >= state.capacity;

    // 窗口内（已完成片 + 当前开放片）的点位与驱动结果聚合。
    let window_points: u64 = state.ring.iter().map(|s| s.points).sum::<u64>()
        + state.cur.as_ref().map(|s| s.points).unwrap_or(0);
    let window_ok: u64 = state.ring.iter().map(|s| s.ok).sum::<u64>()
        + state.cur.as_ref().map(|s| s.ok).unwrap_or(0);
    let window_fail: u64 = state.ring.iter().map(|s| s.fail).sum::<u64>()
        + state.cur.as_ref().map(|s| s.fail).unwrap_or(0);

    // ── 吞吐 ──
    let throughput = if window_points == 0 {
        json!({ "available": false, "reason": "no samples yet" })
    } else {
        // 滑动平均：仅在「已完成」的时间片上求均值（当前开放片未满，不参与均值）。
        let sliding = if ring_len > 0 {
            let sum: u64 = state.ring.iter().map(|s| s.points).sum();
            sum as f64 / (ring_len as f64 * secs as f64)
        } else {
            0.0
        };
        // 瞬时：当前开放片点数 / 该片已流逝秒数（流逝为 0 时记 0，不编造）。
        let now = state.clock.now_ns();
        let instantaneous = match &state.cur {
            Some(s) => {
                let elapsed = (now - s.start_ns) as f64 / NS_PER_SEC as f64;
                if elapsed > 0.0 {
                    s.points as f64 / elapsed
                } else {
                    0.0
                }
            }
            None => 0.0,
        };
        json!({
            "available": true,
            "instantaneous_points_per_sec": instantaneous,
            "sliding_average_points_per_sec": sliding,
            "window_slices": filled.to_string(),
            "slice_seconds": secs.to_string(),
            "ring_full": ring_full,
        })
    };

    // ── 每设备吞吐 ──
    let per_device = if !state.device_seen {
        json!({ "available": false, "reason": "no per-device samples yet" })
    } else {
        let mut agg: HashMap<String, u64> = HashMap::new();
        for s in state.ring.iter().chain(state.cur.iter()) {
            for (dev, c) in &s.per_device {
                let entry = agg.entry(dev.clone()).or_insert(0);
                *entry = entry.saturating_add(*c);
            }
        }
        let span_secs = (filled.max(1) as f64) * secs as f64;
        let mut devices = serde_json::Map::new();
        for (dev, pts) in &agg {
            devices.insert(
                dev.clone(),
                json!({
                    "points_per_sec": *pts as f64 / span_secs,
                    "points": pts.to_string(),
                }),
            );
        }
        json!({ "available": true, "devices": Value::Object(devices) })
    };

    // ── 队列深度 ──
    let queue_depth = match state.queue_depth {
        Some(d) => json!({ "available": true, "depth": d.to_string() }),
        None => json!({ "available": false, "reason": "no queue depth sample yet" }),
    };

    // ── 采集成功率 ──
    let success_rate = if window_ok + window_fail == 0 {
        json!({ "available": false, "reason": "no driver results yet" })
    } else {
        let rate = window_ok as f64 / (window_ok + window_fail) as f64;
        json!({
            "available": true,
            "rate": rate,
            "ok": window_ok.to_string(),
            "fail": window_fail.to_string(),
        })
    };

    // ── 驱动连接状态 ──
    let driver_connection = match state.driver_liveness {
        Some(c) => json!({ "available": true, "connected": c }),
        None => json!({ "available": false, "reason": "no driver liveness signal recorded yet" }),
    };

    // ── 采样窗口说明 ──
    let since_ns = state
        .ring
        .front()
        .map(|s| s.start_ns)
        .or_else(|| state.cur.as_ref().map(|s| s.start_ns));
    let window = json!({
        "slice_seconds": secs.to_string(),
        "capacity": state.capacity.to_string(),
        "filled_slices": filled.to_string(),
        "ring_full": ring_full,
        "since_ns": since_ns.map(|v| v.to_string()).unwrap_or_else(|| "0".to_string()),
    });

    json!({
        "available": window_points > 0
            || window_ok + window_fail > 0
            || state.queue_depth.is_some()
            || state.driver_liveness.is_some()
            || state.device_seen,
        "throughput": throughput,
        "per_device": per_device,
        "queue_depth": queue_depth,
        "success_rate": success_rate,
        "driver_connection": driver_connection,
        "window": window,
    })
}

// ───────────────────────────── 测试辅助 ─────────────────────────────

#[cfg(test)]
pub(crate) fn _reset() {
    let mut g = lock();
    *g = MetricsState::new();
}

#[cfg(test)]
pub(crate) fn _ring_len() -> usize {
    lock().ring.len()
}

#[cfg(test)]
pub(crate) fn _total_slices() -> usize {
    let g = lock();
    g.ring.len() + usize::from(g.cur.is_some())
}

#[cfg(test)]
pub(crate) fn _set_clock(clock: Arc<dyn MetricsClock>) {
    lock().clock = clock;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::offline_queue::ManualClock;
    use std::sync::Mutex as StdMutex;

    /// 串行化所有指标测试（全局单例，避免并行互相污染）。
    static TEST_SERIAL: StdMutex<()> = StdMutex::new(());
    const NS: i64 = 1_000_000_000;

    /// 重置为干净状态并装上手工时钟，返回时钟句柄以便推进时间。
    /// 调用方须先持有 TEST_SERIAL 锁。
    fn setup() -> Arc<ManualClock> {
        _reset();
        let clk = Arc::new(ManualClock::new(0));
        _set_clock(clk.clone());
        configure(DEFAULT_CAPACITY, DEFAULT_SLICE_SECONDS);
        clk
    }

    /// 递归断言：快照里不存在任何绝对值超过 2^53 的 JSON number。
    fn scan_no_overflow(v: &Value) -> bool {
        const LIMIT: f64 = 9_007_199_254_740_993.0; // 2^53 - 1
        match v {
            Value::Number(n) => {
                if let Some(f) = n.as_f64() {
                    (-LIMIT..=LIMIT).contains(&f)
                } else if let Some(u) = n.as_u64() {
                    u <= 9_007_199_254_740_993
                } else if let Some(i) = n.as_i64() {
                    i.unsigned_abs() <= 9_007_199_254_740_993
                } else {
                    false
                }
            }
            Value::Array(a) => a.iter().all(scan_no_overflow),
            Value::Object(o) => o.values().all(scan_no_overflow),
            _ => true,
        }
    }

    #[test]
    fn ring_buffer_evicts_oldest() {
        let _g = TEST_SERIAL.lock().unwrap();
        let clk = setup();
        configure(3, 1);
        record_batch(10); // slice 0
        clk.advance(NS);
        record_batch(10); // slice 1
        clk.advance(NS);
        record_batch(10); // slice 2
        clk.advance(NS);
        record_batch(10); // slice 3 → 淘汰最旧的 slice 0
        assert_eq!(
            _ring_len(),
            3,
            "超过容量后最旧一片必须被淘汰，ring 长度恒等于容量"
        );
        assert_eq!(_total_slices(), 4, "已录入 4 片（3 已完成 + 1 开放）");
    }

    #[test]
    fn sliding_average_correct() {
        let _g = TEST_SERIAL.lock().unwrap();
        let clk = setup();
        configure(10, 1);
        record_batch(100); // slice 0
        clk.advance(NS);
        record_batch(100); // slice 1
        clk.advance(NS);
        record_batch(100); // slice 2（当前开放片）
        let snap = snapshot();
        let tp = &snap["throughput"];
        assert_eq!(tp["available"], true);
        let sliding = tp["sliding_average_points_per_sec"].as_f64().unwrap();
        // 已完成片 = slice0,slice1 → 200 / (2 × 1s) = 100.0
        assert!(
            (sliding - 100.0).abs() < 1e-9,
            "滑动平均应=100，实际={sliding}"
        );
        let inst = tp["instantaneous_points_per_sec"].as_f64().unwrap();
        assert!(inst.is_finite(), "瞬时吞吐必须是有限数");
    }

    #[test]
    fn honest_empty_state_no_samples() {
        let _g = TEST_SERIAL.lock().unwrap();
        let _clk = setup(); // 仅重置，不录入任何样本
        let snap = snapshot();
        assert_eq!(snap["available"], false);
        assert_eq!(snap["throughput"]["available"], false);
        assert!(
            snap["throughput"]["reason"].as_str().is_some(),
            "吞吐未采样必须带真实 reason"
        );
        assert_eq!(snap["per_device"]["available"], false);
        assert!(
            snap["per_device"]["reason"].as_str().is_some(),
            "per_device 未采样必须带 reason"
        );
        assert_eq!(snap["queue_depth"]["available"], false);
        assert_eq!(snap["success_rate"]["available"], false);
        assert_eq!(snap["driver_connection"]["available"], false);
        // 诚实空态下不得出现任何伪造的吞吐数字。
        assert!(snap["throughput"]["instantaneous_points_per_sec"].is_null());
    }

    #[test]
    fn snapshot_no_number_exceeds_2_53() {
        let _g = TEST_SERIAL.lock().unwrap();
        let _clk = setup();
        // 用真实量级的采集数据 + 一个刻意的大数计数（队列深度）验证大数走字符串。
        record_queue_depth(u64::MAX);
        record_batch(100);
        record_driver_result(true);
        record_driver_result(true);
        record_driver_result(false);
        record_point_device("devA", 30);
        let snap = snapshot();
        // 大数计数必须序列化为字符串，而非丢精度的 number。
        assert_eq!(
            snap["queue_depth"]["depth"].as_str(),
            Some("18446744073709551615"),
            "u64 计数必须是字符串"
        );
        assert!(
            scan_no_overflow(&snap),
            "快照里不得出现绝对值超过 2^53 的数字"
        );
    }

    #[test]
    fn queue_depth_available_after_record() {
        let _g = TEST_SERIAL.lock().unwrap();
        let _clk = setup();
        assert_eq!(snapshot()["queue_depth"]["available"], false);
        record_queue_depth(42);
        let snap = snapshot();
        assert_eq!(snap["queue_depth"]["available"], true);
        assert_eq!(snap["queue_depth"]["depth"].as_str(), Some("42"));
    }

    #[test]
    fn success_rate_computed() {
        let _g = TEST_SERIAL.lock().unwrap();
        let _clk = setup();
        record_driver_result(true);
        record_driver_result(true);
        record_driver_result(false);
        let snap = snapshot();
        assert_eq!(snap["success_rate"]["available"], true);
        assert_eq!(snap["success_rate"]["ok"].as_str(), Some("2"));
        assert_eq!(snap["success_rate"]["fail"].as_str(), Some("1"));
        let rate = snap["success_rate"]["rate"].as_f64().unwrap();
        assert!((rate - 2.0 / 3.0).abs() < 1e-9, "成功率应=2/3，实际={rate}");
    }
}
