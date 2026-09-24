//! 北向运行期接线（task 19 + task 54 的「最后一跳」）。
//!
//! ## 为什么需要这一层
//! [`crate::north::mqtt`] 提供了 [`MqttClient`] + [`NorthOutlet`]（有界发送队列 /
//! PUBACK 门控确认 / 补发幂等 / 审计取走上报），但它们**没有任何运行期调用方**：
//! 原语齐全却跑不起来。本模块把「按 `[[outlets]]` 建出口 → 起驱动任务 → 停机收口」
//! 这一跳做完，并把出口注册表挂到 `DaemonShared`（见 `crate::bootstrap`），
//! 使采集侧能在运行期真正投递数据。
//!
//! ## 每个出口的驱动循环
//! 固定两步，每轮一拍：
//! 1. [`MqttClient::poll_event`]：推进 rumqttc 事件循环。`PUBACK` / `PUBCOMP` 到达时
//!    该方法内部回收发送队列在途窗口（**只有确认到达才回收**）；出错则
//!    [`MqttClient::backoff`] 后继续（rumqttc 语义：继续 poll 即自动重连，
//!    会话恢复由 CONNACK 的 `session_present` 观测）。
//! 2. [`MqttClient::pump`]：发送队列一轮 → 补发一轮 → 审计取走上报。
//!
//! ## 红线
//! - **采集路径永不阻塞**：采集侧只调 [`NorthRuntime::submit`]（同步、无 await、一次整数比较 + 一次有界落盘调用），驱动任务在独立 tokio task 里跑。
//! - 每个出口各自持有 `AuditLog`（有界），共用同一 [`OfflineQueue`] 与同一 [`Clock`]。
//! - **TLS / mTLS 出口（task 25）**：`[[outlets]]` 的 `ca_cert_path` /
//!   `client_cert_path` / `client_key_path` / `server_name` / `alpn` 经
//!   [`endpoint_from_outlet`] 映射为 [`TlsConfig`] 挂到端点；规则 fail-closed
//!   （缺 CA / 证书私钥不配对 / 路径不存在 / `server_name` 与 host 不符 → 该出口
//!   被 `error!` 拒绝并计入 [`NorthRuntimeStats::skipped`]），**绝不**静默降级明文。
//! - 停机 = `abort()` 全部驱动任务（幂等、不阻塞宽限期）。

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tracing::{error, info, warn};

use crate::backpressure::PushOutcome;
use crate::config::OutletConfig;
use crate::error::{DaemonError, DaemonResult};
use crate::north::mqtt::{
    AuditSink, EndpointConfig, MqttClient, NorthOutlet, TlsConfig, DEFAULT_MQTTS_PORT,
    DEFAULT_MQTT_PORT,
};
use crate::offline_queue::{Clock, OfflineQueue};

/// 北向驱动默认轮询周期。
///
/// 取 200 ms：QoS1 在途水位 32 时对应约 160 条/秒的确认吞吐（设计文档 §4），
/// 同时保证 PUBACK 回收与补发推进的延迟远小于采集周期。
pub const DEFAULT_NORTH_TICK: Duration = Duration::from_millis(200);

/// 取锁并在中毒时取回内部数据（**绝不 panic**）。
fn lock_or_recover<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => PoisonError::into_inner(poisoned),
    }
}

fn config_err(message: impl Into<String>) -> DaemonError {
    DaemonError::ConfigError(message.into())
}

/// 拆分 `host[:port]`，支持 IPv6 字面量 `[::1]:1883`；缺省端口按 scheme 取默认值。
fn split_host_port(raw: &str, default_port: u16) -> DaemonResult<(String, u16)> {
    let raw = raw.trim().trim_end_matches('/');
    // IPv6 字面量。
    if let Some(rest) = raw.strip_prefix('[') {
        let (host, tail) = rest
            .split_once(']')
            .ok_or_else(|| config_err(format!("outlet broker: unclosed IPv6 literal `{raw}`")))?;
        let port = match tail.strip_prefix(':') {
            Some(text) => parse_port(text)?,
            None => default_port,
        };
        return Ok((host.to_string(), port));
    }
    match raw.rsplit_once(':') {
        // 多个 `:` 且不带方括号 → 无法判定 host/port 边界，显式拒绝（不猜）。
        Some((host, text)) if !host.contains(':') => Ok((host.to_string(), parse_port(text)?)),
        Some(_) => Err(config_err(format!(
            "outlet broker: ambiguous `{raw}` (write IPv6 as `[addr]:port`)"
        ))),
        None => Ok((raw.to_string(), default_port)),
    }
}

fn parse_port(text: &str) -> DaemonResult<u16> {
    text.trim()
        .parse::<u16>()
        .map_err(|_| config_err(format!("outlet broker: invalid port `{text}`")))
        .and_then(|port| {
            if port == 0 {
                Err(config_err("outlet broker: port must be 1-65535 (got 0)"))
            } else {
                Ok(port)
            }
        })
}

/// `[[outlets]]` 行 → MQTT 端点（唯一转换点）。
///
/// 接受 `mqtt://host:port` / `mqtts://host:port` / `host:port` / `host`；
/// 缺省端口 `mqtt` → 1883、`mqtts` → 8883。
///
/// ## TLS / mTLS（task 25）
/// 出口启用 TLS 时**必须**提供可校验的信任锚（`ca_cert_path`），并可选提供
/// 成对的客户端证书（`client_cert_path` + `client_key_path`）以启用 mTLS。
/// 全部规则 **fail-closed**：任一违规返回 [`DaemonError::ConfigError`]（含出口名），
/// **绝不**静默降级为明文、**绝不**跳过证书校验。
///
/// 规则清单：
/// 1. `tls = true` 但 scheme 为 `mqtt://`（或省略 scheme）→ 报错（不猜意图）。
/// 2. TLS 出口（`mqtts://` 或 `tls = true`）缺 `ca_cert_path` → 报错。
/// 3. `client_cert_path` / `client_key_path` 出现任一个时**必须成对**，否则报错。
/// 4. 上述 PEM 路径在**构建出口时**即校验存在（路径写错不留到运行期）。
/// 5. TLS 字段（`ca_cert_path` / `client_cert_path` / `client_key_path` /
///    `server_name` / `alpn`）存在但 TLS 未启用 → 报错（防止「配了 CA 却忘开
///    TLS」被静默当作明文出口）。
/// 6. `server_name` 作为 SNI 覆盖：当前传输层（rumqttc）以 broker host 派生
///    服务端名，**无法**透传独立 SNI——若 `server_name` 与 broker host 不一致则
///    报错（不静默忽略安全相关配置），一致时视为无操作。
/// 7. 非 TLS 出口（`mqtt://` 且 `tls = false`）行为与既往完全一致。
///
/// # Errors
/// - scheme 不是 `mqtt` / `mqtts`；
/// - host 为空 / 端口非法或为 0；
/// - 上述任一 TLS 规则违规。
pub fn endpoint_from_outlet(outlet: &OutletConfig) -> DaemonResult<EndpointConfig> {
    let raw = outlet.broker.trim();
    let (scheme, rest) = match raw.split_once("://") {
        Some((scheme, rest)) => (scheme.to_ascii_lowercase(), rest),
        None => ("mqtt".to_string(), raw),
    };
    let tls_by_scheme = match scheme.as_str() {
        "mqtt" => false,
        "mqtts" => true,
        other => {
            return Err(config_err(format!(
                "outlet `{}`: unsupported broker scheme `{other}://` \
                 (expected `mqtt://` or `mqtts://`)",
                outlet.name
            )))
        }
    };

    // 规则 1：`tls = true` 与 `mqtt://` 冲突 → 显式报错，绝不猜测意图。
    if outlet.tls && !tls_by_scheme {
        return Err(config_err(format!(
            "outlet `{}`: tls = true but broker scheme is `mqtt://` (plaintext); \
             use `mqtts://` for a TLS outlet — refusing to guess the intended transport",
            outlet.name
        )));
    }
    let tls_enabled = tls_by_scheme || outlet.tls;

    let default_port = if tls_by_scheme {
        DEFAULT_MQTTS_PORT
    } else {
        DEFAULT_MQTT_PORT
    };
    let (host, port) = split_host_port(rest, default_port)?;
    if host.trim().is_empty() {
        return Err(config_err(format!(
            "outlet `{}`: broker host is empty (`{}`)",
            outlet.name, outlet.broker
        )));
    }

    let endpoint = EndpointConfig::new(outlet.name.clone(), host.clone(), port)
        .with_qos(outlet.qos)?
        .with_topic_prefix(outlet.topic_prefix.clone())
        .with_encoding(outlet.encoding.into());

    if !tls_enabled {
        // 规则 5：非 TLS 出口若携带 TLS 字段 → 报错（不静默忽略安全配置）。
        let stray = [
            ("ca_cert_path", outlet.ca_cert_path.is_some()),
            ("client_cert_path", outlet.client_cert_path.is_some()),
            ("client_key_path", outlet.client_key_path.is_some()),
            ("server_name", outlet.server_name.is_some()),
            ("alpn", !outlet.alpn.is_empty()),
        ];
        if let Some((field, _)) = stray.iter().find(|(_, present)| *present) {
            return Err(config_err(format!(
                "outlet `{}`: `{field}` is set but TLS is not enabled; use `mqtts://` \
                 or `tls = true` — refusing to silently send plaintext",
                outlet.name
            )));
        }
        return Ok(endpoint);
    }

    let tls = build_tls_config(outlet, &host)?;
    Ok(endpoint.with_tls(tls))
}

/// 由 `[[outlets]]` 行构造 [`TlsConfig`]（TLS 出口专用；规则 2–6 的实现）。
///
/// 只读 IO（PEM 路径存在性检查），无隐藏可变状态；不读取/解析 PEM 内容——
/// 内容合法性由 [`TlsConfig::to_transport`] / [`EndpointConfig::validate`] 负责。
fn build_tls_config(outlet: &OutletConfig, broker_host: &str) -> DaemonResult<TlsConfig> {
    // 规则 2：CA 必填。
    let ca = match outlet.ca_cert_path.as_deref().map(str::trim) {
        Some(path) if !path.is_empty() => path.to_string(),
        _ => {
            return Err(config_err(format!(
                "outlet `{}`: TLS is enabled but `ca_cert_path` is missing; a verifiable \
                 TLS session requires the broker CA certificate (skipping certificate \
                 verification is not supported)",
                outlet.name
            )))
        }
    };

    let cert = non_empty(outlet.client_cert_path.as_deref());
    let key = non_empty(outlet.client_key_path.as_deref());

    // 规则 3：客户端证书与私钥必须成对。
    match (&cert, &key) {
        (Some(_), None) => {
            return Err(config_err(format!(
                "outlet `{}`: `client_cert_path` is set but `client_key_path` is missing \
                 (mTLS requires both)",
                outlet.name
            )))
        }
        (None, Some(_)) => {
            return Err(config_err(format!(
                "outlet `{}`: `client_key_path` is set but `client_cert_path` is missing \
                 (mTLS requires both)",
                outlet.name
            )))
        }
        _ => {}
    }

    // 规则 4：路径存在性（构建期 fail-closed）。
    ensure_cert_file_exists(outlet, "ca_cert_path", &ca)?;
    if let Some(path) = &cert {
        ensure_cert_file_exists(outlet, "client_cert_path", path)?;
    }
    if let Some(path) = &key {
        ensure_cert_file_exists(outlet, "client_key_path", path)?;
    }

    // 规则 6：SNI 覆盖当前无法透传 → 与 broker host 不一致即报错（不静默忽略）。
    if let Some(sni) = non_empty(outlet.server_name.as_deref()) {
        if !sni.eq_ignore_ascii_case(broker_host) {
            return Err(config_err(format!(
                "outlet `{}`: `server_name` = `{sni}` differs from broker host `{broker_host}`; \
                 an independent SNI override is not supported by the MQTT transport in V1 — \
                 set `broker` host to the certificate's server name instead",
                outlet.name
            )));
        }
    }

    let mut tls = match (&cert, &key) {
        (Some(cert), Some(key)) => TlsConfig::mtls(ca, cert.clone(), key.clone()),
        _ => TlsConfig::ca_only(ca),
    };
    tls.alpn = outlet
        .alpn
        .iter()
        .map(|proto| proto.as_bytes().to_vec())
        .collect();
    Ok(tls)
}

/// 去首尾空白并过滤空串（`None` / 空白 = 未提供）。
fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// 校验 PEM 路径指向一个**已存在且为文件**的路径（fail-closed）。
fn ensure_cert_file_exists(outlet: &OutletConfig, field: &str, path: &str) -> DaemonResult<()> {
    if Path::new(path).is_file() {
        Ok(())
    } else {
        Err(config_err(format!(
            "outlet `{}`: `{field}` path `{path}` does not exist or is not a file",
            outlet.name
        )))
    }
}

/// 北向运行期装配输入（**不新增配置段**：出口来自 `[[outlets]]`，其余由装配方注入）。
pub struct NorthRuntimeConfig {
    /// 共享离线队列（落盘降级目标 + 补发源）。
    ///
    /// ⚠ 必须与采集侧入队用的是**同一个实例**，否则「发送队列超限 → 落盘降级」
    /// 的批次不会被补发路径看到。
    pub queue: Arc<OfflineQueue>,
    /// 时钟（审计时间戳 / `PendingSend::enqueued_ns`）。
    pub clock: Arc<dyn Clock>,
    /// 审计上报出口（`AuditLog::drain()` 的落点）。
    pub audit_sink: Arc<dyn AuditSink>,
    /// 驱动轮询周期。
    pub tick: Duration,
}

impl NorthRuntimeConfig {
    /// 按默认轮询周期构造。
    #[must_use]
    pub fn new(
        queue: Arc<OfflineQueue>,
        clock: Arc<dyn Clock>,
        audit_sink: Arc<dyn AuditSink>,
    ) -> Self {
        Self {
            queue,
            clock,
            audit_sink,
            tick: DEFAULT_NORTH_TICK,
        }
    }

    /// 覆盖驱动轮询周期。
    #[must_use]
    pub fn with_tick(mut self, tick: Duration) -> Self {
        self.tick = tick;
        self
    }
}

impl std::fmt::Debug for NorthRuntimeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NorthRuntimeConfig")
            .field("queue_db", &self.queue.queue_db_path())
            .field("gateway_id", &self.queue.gateway_id())
            .field("tick", &self.tick)
            .finish_non_exhaustive()
    }
}

/// 北向运行期统计（可观测）。
///
/// [`Self::poll_cycles`] 是「驱动任务真的在跑」的证据：接线被摘掉时它恒为 0。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NorthRuntimeStats {
    /// 实际启动的出口数。
    pub outlets: usize,
    /// 因配置非法 / TLS 被拒绝而未启动的出口数（> 0 需运维处理）。
    pub skipped: u64,
    /// 驱动循环轮次（各出口累加）。
    pub poll_cycles: u64,
    /// `poll_event` 成功次数。
    pub polls_ok: u64,
    /// `poll_event` 失败次数（已按重连退避重试）。
    pub polls_failed: u64,
    /// 完整泵轮次（发送 + 补发 + 审计）。
    pub pumps: u64,
    /// 泵返回错误次数。
    pub pump_errors: u64,
    /// 补发成功条数。
    pub replayed: u64,
    /// 发送队列入内存条数（各出口合计）。
    pub admitted: u64,
    /// 发送队列落盘降级条数。
    pub spilled: u64,
    /// 发送队列硬上限拒绝条数（数据已交还调用方）。
    pub rejected: u64,
    /// 审计累计产生条数。
    pub audit_emitted: u64,
    /// 审计因环满被淘汰条数（> 0 = 审计消费变慢）。
    pub audit_dropped: u64,
}

/// 驱动任务写入的原子计数器。
#[derive(Debug, Default)]
struct Counters {
    skipped: AtomicU64,
    poll_cycles: AtomicU64,
    polls_ok: AtomicU64,
    polls_failed: AtomicU64,
    pumps: AtomicU64,
    pump_errors: AtomicU64,
    replayed: AtomicU64,
}

impl Counters {
    fn bump(slot: &AtomicU64) {
        slot.fetch_add(1, Ordering::Relaxed);
    }

    fn add(slot: &AtomicU64, delta: u64) {
        slot.fetch_add(delta, Ordering::Relaxed);
    }

    fn get(slot: &AtomicU64) -> u64 {
        slot.load(Ordering::Relaxed)
    }
}

/// 一个已启动出口的运行句柄。
struct OutletRuntime {
    /// 出口名（与 `[[outlets]].name` 一致）。
    name: String,
    /// 背压接线束（采集侧发布入口 + 观测）。
    outlet: Arc<NorthOutlet>,
}

/// 北向运行期：按 `[[outlets]]` 为每个出口构造 `MqttClient` + `NorthOutlet`
/// （共享同一 `OfflineQueue` / `Clock` / 审计出口）并启动驱动任务。
pub struct NorthRuntime {
    /// 已启动出口（保序）。
    outlets: Vec<OutletRuntime>,
    /// 出口名 → 下标（O(1) 查表）。
    by_name: HashMap<String, usize>,
    /// 共享离线队列。
    queue: Arc<OfflineQueue>,
    /// 计数器。
    counters: Arc<Counters>,
    /// 驱动任务句柄（停机时全部 abort）。
    tasks: StdMutex<Vec<JoinHandle<()>>>,
}

impl NorthRuntime {
    /// 按出口配置构造并启动运行期（**不阻塞**：连接由驱动任务首次 poll 触发）。
    ///
    /// 非法 / TLS 出口不会中断启动：记 `error!`、计入 [`NorthRuntimeStats::skipped`]，
    /// 其余出口继续——采集侧不因单个出口配置错误而停摆。
    ///
    /// `shutdown`：停机信号（`DaemonShared::subscribe_shutdown()`）；收到即退出驱动循环。
    #[must_use]
    pub fn start(
        gateway_id: &str,
        outlets: &[OutletConfig],
        cfg: NorthRuntimeConfig,
        shutdown: watch::Receiver<bool>,
    ) -> Self {
        let counters = Arc::new(Counters::default());
        let mut started: Vec<OutletRuntime> = Vec::new();
        let mut by_name: HashMap<String, usize> = HashMap::new();
        let mut tasks: Vec<JoinHandle<()>> = Vec::new();

        for outlet_cfg in outlets {
            if by_name.contains_key(&outlet_cfg.name) {
                Counters::bump(&counters.skipped);
                error!(
                    outlet = %outlet_cfg.name,
                    "north runtime: duplicate outlet name; skipped (names are the registry key)"
                );
                continue;
            }
            let endpoint = match endpoint_from_outlet(outlet_cfg) {
                Ok(endpoint) => endpoint,
                Err(err) => {
                    Counters::bump(&counters.skipped);
                    error!(
                        outlet = %outlet_cfg.name,
                        error = %err,
                        "north runtime: outlet rejected; skipped (see `[[outlets]]` broker/tls fields)"
                    );
                    continue;
                }
            };
            let client = match MqttClient::new(endpoint) {
                Ok(client) => client,
                Err(err) => {
                    Counters::bump(&counters.skipped);
                    error!(
                        outlet = %outlet_cfg.name,
                        error = %err,
                        "north runtime: MQTT client build failed; skipped"
                    );
                    continue;
                }
            };
            // 每个出口一个审计环（有界）；发送 / 补发 / 消费闸门三者共享，
            // 形成「超限落盘 → 幂等补发 → 审计统一上报」的闭环。
            let audit_log = Arc::new(crate::backpressure::AuditLog::default());
            let outlet = Arc::new(NorthOutlet::with_audit_log(
                gateway_id.to_string(),
                Arc::clone(&cfg.queue),
                Arc::clone(&cfg.clock),
                audit_log,
                Arc::clone(&cfg.audit_sink),
            ));
            let task = spawn_driver(
                outlet_cfg.name.clone(),
                outlet_cfg.topic_prefix.clone(),
                client,
                Arc::clone(&outlet),
                Arc::clone(&counters),
                shutdown.clone(),
                cfg.tick,
            );
            by_name.insert(outlet_cfg.name.clone(), started.len());
            started.push(OutletRuntime {
                name: outlet_cfg.name.clone(),
                outlet,
            });
            tasks.push(task);
        }

        NorthRuntime {
            outlets: started,
            by_name,
            queue: cfg.queue,
            counters,
            tasks: StdMutex::new(tasks),
        }
    }

    /// 出口数（实际启动的）。
    #[must_use]
    pub fn outlet_count(&self) -> usize {
        self.outlets.len()
    }

    /// 出口名清单（保序）。
    #[must_use]
    pub fn outlet_names(&self) -> Vec<&str> {
        self.outlets.iter().map(|o| o.name.as_str()).collect()
    }

    /// 按名取背压接线束（采集侧发布入口 / 观测）。
    #[must_use]
    pub fn outlet(&self, name: &str) -> Option<&Arc<NorthOutlet>> {
        self.by_name
            .get(name)
            .and_then(|index| self.outlets.get(*index))
            .map(|entry| &entry.outlet)
    }

    /// 提交一个批次到指定出口的有界发送队列（**同步、永不阻塞采集路径**）。
    ///
    /// # Errors
    /// 出口名未知 → `ConfigError`（2000）。硬上限且落盘失败**不是错误**：
    /// 返回 [`PushOutcome::Rejected`] 并交还数据（已记审计）。
    pub fn submit(&self, name: &str, seq: u64, payload: Vec<u8>) -> DaemonResult<PushOutcome> {
        let outlet = self.outlet(name).ok_or_else(|| {
            config_err(format!(
                "north runtime: unknown outlet `{name}` (started: {:?})",
                self.outlet_names()
            ))
        })?;
        Ok(outlet.send().push(seq, payload))
    }

    /// 共享离线队列（补发源 / 落盘降级目标）。
    #[must_use]
    pub fn queue(&self) -> &Arc<OfflineQueue> {
        &self.queue
    }

    /// 统计快照。
    #[must_use]
    pub fn stats(&self) -> NorthRuntimeStats {
        let mut stats = NorthRuntimeStats {
            outlets: self.outlets.len(),
            skipped: Counters::get(&self.counters.skipped),
            poll_cycles: Counters::get(&self.counters.poll_cycles),
            polls_ok: Counters::get(&self.counters.polls_ok),
            polls_failed: Counters::get(&self.counters.polls_failed),
            pumps: Counters::get(&self.counters.pumps),
            pump_errors: Counters::get(&self.counters.pump_errors),
            replayed: Counters::get(&self.counters.replayed),
            ..NorthRuntimeStats::default()
        };
        for entry in &self.outlets {
            let send = entry.outlet.send().stats();
            stats.admitted = stats.admitted.saturating_add(send.admitted);
            stats.spilled = stats.spilled.saturating_add(send.spilled);
            stats.rejected = stats.rejected.saturating_add(send.rejected);
            let audit = entry.outlet.audit();
            stats.audit_emitted = stats.audit_emitted.saturating_add(audit.emitted());
            stats.audit_dropped = stats.audit_dropped.saturating_add(audit.dropped());
        }
        stats
    }

    /// 存活驱动任务数。
    #[must_use]
    pub fn running_tasks(&self) -> usize {
        lock_or_recover(&self.tasks)
            .iter()
            .filter(|task| !task.is_finished())
            .count()
    }

    /// 停机：`abort()` 全部驱动任务（幂等、不阻塞）。
    ///
    /// 驱动任务被中止前已由 `SendQueue` 的落盘降级保证数据安全（内存中条目
    /// 仍由队列自身的内存/磁盘双写兜底）。
    pub fn shutdown(&self) {
        for task in lock_or_recover(&self.tasks).drain(..) {
            task.abort();
        }
    }
}

impl std::fmt::Debug for NorthRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NorthRuntime")
            .field("outlets", &self.outlet_names())
            .field("running_tasks", &self.running_tasks())
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

/// 单个出口的驱动任务：每拍「推进事件循环 → 一轮泵」。
fn spawn_driver(
    name: String,
    topic_prefix: String,
    client: MqttClient,
    outlet: Arc<NorthOutlet>,
    counters: Arc<Counters>,
    mut shutdown: watch::Receiver<bool>,
    tick: Duration,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // 驱动任务独占该客户端（无其它持有者），按值持有并只用 `&mut self` 方法：
        // `MqttClient` 因 rumqttc `EventLoop` 内的 `!Sync` 传输而为 `!Sync`，
        // 任何 `&self` 方法跨 await 都会让本 future 非 `Send`（无法 `tokio::spawn`）。
        let mut client = client;
        let mut ticker = tokio::time::interval(tick);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // interval 首拍立即完成，先消费掉再进循环。
        ticker.tick().await;
        loop {
            if *shutdown.borrow_and_update() {
                break;
            }
            tokio::select! {
                _ = ticker.tick() => {}
                _ = shutdown.changed() => break,
            }
            Counters::bump(&counters.poll_cycles);

            // ① 推进事件循环：PUBACK/PUBCOMP → 回收发送队列在途窗口；
            //    出错 → 按退避重试（继续 poll 即自动重连）。
            match client.poll_event().await {
                Ok(_) => Counters::bump(&counters.polls_ok),
                Err(err) => {
                    Counters::bump(&counters.polls_failed);
                    warn!(
                        outlet = %name,
                        error = %err,
                        "north driver: poll failed; backing off before reconnect"
                    );
                    client.backoff().await;
                    continue;
                }
            }
            // ② 发送一轮 + 补发一轮 + 审计取走上报。
            match client.pump(&topic_prefix).await {
                Ok(report) => {
                    Counters::bump(&counters.pumps);
                    if report.replay.sent > 0 {
                        Counters::add(&counters.replayed, report.replay.sent as u64);
                    }
                    if report.audit_emitted > 0 {
                        info!(
                            outlet = %name,
                            audit_emitted = report.audit_emitted,
                            "north driver: backpressure audit forwarded"
                        );
                    }
                }
                Err(err) => {
                    Counters::bump(&counters.pump_errors);
                    warn!(outlet = %name, error = %err, "north driver: pump failed");
                }
            }
        }
        // 收口：把离线队列内存中的降级数据落盘（best-effort，失败仅告警）。
        if let Err(err) = outlet.replay().queue().flush() {
            warn!(outlet = %name, error = %err, "north driver: final queue flush failed");
        }
        info!(outlet = %name, "north driver: stopped");
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OutletEncoding;

    /// 构造一个仅含最小显式字段的出口（TLS 字段全空），供各规则单测按需覆写。
    fn base_outlet(name: &str, broker: &str) -> OutletConfig {
        OutletConfig {
            name: name.to_string(),
            broker: broker.to_string(),
            topic_prefix: "telemetry".to_string(),
            qos: 1,
            tls: false,
            ca_cert_path: None,
            client_cert_path: None,
            client_key_path: None,
            server_name: None,
            alpn: Vec::new(),
            encoding: OutletEncoding::Protobuf,
        }
    }

    /// 写一个占位 PEM 文件（仅用于「路径存在性」校验；内容由 `to_transport` 校验）。
    fn write_pem(dir: &Path, file: &str) -> String {
        let path = dir.join(file);
        std::fs::write(
            &path,
            b"-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n",
        )
        .expect("write pem placeholder");
        path.to_str().expect("utf8 path").to_string()
    }

    /// 规则 1：`tls = true` + `mqtt://` → 报错（含出口名，提示用 `mqtts://`）。
    #[test]
    fn tls_true_with_plaintext_scheme_is_rejected() {
        let mut outlet = base_outlet("north-x", "mqtt://broker.local:1883");
        outlet.tls = true;
        let err = endpoint_from_outlet(&outlet).expect_err("must reject");
        assert_eq!(err.error_code(), crate::error::ERR_CONFIG);
        let msg = err.to_string();
        assert!(msg.contains("north-x"), "msg={msg}");
        assert!(msg.contains("mqtts://"), "msg={msg}");
    }

    /// 规则 2：`mqtts://` 缺 `ca_cert_path` → 报错（含出口名与字段名）。
    #[test]
    fn tls_mqtts_without_ca_is_rejected() {
        let outlet = base_outlet("north-ca", "mqtts://broker.local:8883");
        let err = endpoint_from_outlet(&outlet).expect_err("must reject");
        let msg = err.to_string();
        assert!(msg.contains("north-ca"), "msg={msg}");
        assert!(msg.contains("ca_cert_path"), "msg={msg}");
    }

    /// 规则 3：`client_cert_path` 有、`client_key_path` 无 → 报错。
    #[test]
    fn tls_client_cert_without_key_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ca = write_pem(dir.path(), "ca.crt");
        let cert = write_pem(dir.path(), "client.crt");
        let mut outlet = base_outlet("pair-a", "mqtts://broker.local:8883");
        outlet.ca_cert_path = Some(ca);
        outlet.client_cert_path = Some(cert);
        let err = endpoint_from_outlet(&outlet).expect_err("must reject");
        let msg = err.to_string();
        assert!(msg.contains("pair-a"), "msg={msg}");
        assert!(msg.contains("client_key_path"), "msg={msg}");
    }

    /// 规则 3（反向）：`client_key_path` 有、`client_cert_path` 无 → 报错。
    #[test]
    fn tls_client_key_without_cert_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ca = write_pem(dir.path(), "ca.crt");
        let key = write_pem(dir.path(), "client.key");
        let mut outlet = base_outlet("pair-b", "mqtts://broker.local:8883");
        outlet.ca_cert_path = Some(ca);
        outlet.client_key_path = Some(key);
        let err = endpoint_from_outlet(&outlet).expect_err("must reject");
        let msg = err.to_string();
        assert!(msg.contains("pair-b"), "msg={msg}");
        assert!(msg.contains("client_cert_path"), "msg={msg}");
    }

    /// 规则 4：CA 路径指向不存在的文件 → 报错（含出口名、字段名与路径）。
    #[test]
    fn tls_missing_ca_file_is_rejected() {
        let mut outlet = base_outlet("north-miss", "mqtts://broker.local:8883");
        outlet.ca_cert_path = Some("/definitely/not/here/ca.crt".to_string());
        let err = endpoint_from_outlet(&outlet).expect_err("must reject");
        let msg = err.to_string();
        assert!(msg.contains("north-miss"), "msg={msg}");
        assert!(msg.contains("ca_cert_path"), "msg={msg}");
        assert!(msg.contains("/definitely/not/here/ca.crt"), "msg={msg}");
    }

    /// 规则 5：TLS 字段存在但 TLS 未启用（`mqtt://` + `tls = false`）→ 报错。
    #[test]
    fn tls_fields_without_tls_enabled_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ca = write_pem(dir.path(), "ca.crt");
        let mut outlet = base_outlet("north-stray", "mqtt://broker.local:1883");
        outlet.ca_cert_path = Some(ca);
        let err = endpoint_from_outlet(&outlet).expect_err("must reject");
        let msg = err.to_string();
        assert!(msg.contains("north-stray"), "msg={msg}");
        assert!(msg.contains("ca_cert_path"), "msg={msg}");
    }

    /// 规则 6：`server_name` 与 broker host 不一致 → 报错（不静默忽略 SNI 覆盖）。
    #[test]
    fn tls_server_name_mismatch_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ca = write_pem(dir.path(), "ca.crt");
        let mut outlet = base_outlet("north-sni", "mqtts://10.0.0.5:8883");
        outlet.ca_cert_path = Some(ca);
        outlet.server_name = Some("broker.internal".to_string());
        let err = endpoint_from_outlet(&outlet).expect_err("must reject");
        let msg = err.to_string();
        assert!(msg.contains("north-sni"), "msg={msg}");
        assert!(msg.contains("server_name"), "msg={msg}");
    }

    /// 正例：完整 mTLS（ca + client_cert + client_key）→ 成功，且 `TlsConfig`
    /// 三字段与输入一致（证明 endpoint 确实挂上了 TLS，`validate()` 才会校验）。
    #[test]
    fn full_mtls_builds_endpoint_with_tls() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ca = write_pem(dir.path(), "ca.crt");
        let cert = write_pem(dir.path(), "client.crt");
        let key = write_pem(dir.path(), "client.key");
        let mut outlet = base_outlet("north-mtls", "mqtts://broker.local:8883");
        outlet.ca_cert_path = Some(ca.clone());
        outlet.client_cert_path = Some(cert.clone());
        outlet.client_key_path = Some(key.clone());
        outlet.alpn = vec!["mqtt".to_string()];

        let endpoint = endpoint_from_outlet(&outlet).expect("mtls endpoint");
        assert_eq!(endpoint.broker, "broker.local");
        assert_eq!(endpoint.port, 8883);
        endpoint
            .validate()
            .expect("endpoint carries a valid TlsConfig");

        let tls = endpoint.tls.clone().expect("tls attached");
        assert_eq!(tls.ca_cert_path.as_deref(), Some(Path::new(&ca)));
        assert_eq!(tls.client_cert_path.as_deref(), Some(Path::new(&cert)));
        assert_eq!(tls.client_key_path.as_deref(), Some(Path::new(&key)));
        assert_eq!(tls.alpn, vec![b"mqtt".to_vec()]);
    }

    /// 正例：仅 CA（`ca_only` 语义）→ 成功、无客户端证书、缺省端口 = 8883。
    #[test]
    fn ca_only_builds_tls_without_client_cert() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ca = write_pem(dir.path(), "ca.crt");
        let mut outlet = base_outlet("north-caonly", "mqtts://broker.local");
        outlet.ca_cert_path = Some(ca.clone());

        let endpoint = endpoint_from_outlet(&outlet).expect("ca-only endpoint");
        assert_eq!(endpoint.port, DEFAULT_MQTTS_PORT);
        let tls = endpoint.tls.clone().expect("tls attached");
        assert_eq!(tls.ca_cert_path.as_deref(), Some(Path::new(&ca)));
        assert!(tls.client_cert_path.is_none());
        assert!(tls.client_key_path.is_none());
        assert!(tls.alpn.is_empty());
    }

    /// 规则 7：非 TLS 出口行为与既往一致（明文、无 `TlsConfig`、端口 1883）。
    #[test]
    fn tls_plaintext_outlet_is_unchanged() {
        let outlet = base_outlet("plain", "mqtt://broker.local:1883");
        let endpoint = endpoint_from_outlet(&outlet).expect("plain endpoint");
        assert_eq!(endpoint.broker, "broker.local");
        assert_eq!(endpoint.port, 1883);
        assert!(endpoint.tls.is_none());
    }

    /// `tls = true` + `mqtts://`（显式双重声明，语义一致）应被接受。
    #[test]
    fn tls_true_with_mqtts_scheme_is_accepted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ca = write_pem(dir.path(), "ca.crt");
        let mut outlet = base_outlet("north-both", "mqtts://broker.local:8883");
        outlet.tls = true;
        outlet.ca_cert_path = Some(ca);
        let endpoint = endpoint_from_outlet(&outlet).expect("accepted");
        assert!(endpoint.tls.is_some());
    }
}
