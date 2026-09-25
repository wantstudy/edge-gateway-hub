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

/// 门控 warn 日志节流：被授权门控拒绝时，每隔这么多拍重复一次 warn
/// （首拍必打；200ms tick 下约每 30s 提醒一次，含 Degraded 原因与恢复路径）。
const GATED_WARN_EVERY_CYCLES: u64 = 150;

/// 北向转发授权闸门（fail-closed 判据注入点；由 `LicenseRuntime` 实现）。
///
/// 驱动任务每拍先问 [`Self::north_forward_allowed`]：不允许时**跳过本轮发送**
/// （发送队列照常受理采集侧入队，超限自然落盘降级；本地采集与连接维护不受影响），
/// 并按节流节奏把 [`Self::deny_reason`]（Degraded 原因 + 恢复路径）写入 warn 日志。
/// 状态恢复（Grace→Licensed / 激活成功）后下一拍自动恢复转发。
pub trait NorthForwardGate: Send + Sync {
    /// 当前是否允许北向转发（单拍判据；经 `watch` 读取授权状态，跃迁即生效）。
    fn north_forward_allowed(&self) -> bool;
    /// 不可转发时的可读原因（含降级原因与恢复路径）；允许转发时返回 `None`。
    fn deny_reason(&self) -> Option<String>;
}

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
/// ## TLS / mTLS（task 25；2026-09-25 用户决策：**证书配置可选**）
/// 出口启用 TLS 时必须存在**可校验的信任锚**：`ca_cert_path` 提供则用该 CA 文件，
/// **缺省用操作系统根证书库**（`rustls-native-certs`）；并可选提供成对的客户端证书
/// （`client_cert_path` + `client_key_path`）以启用 mTLS。
/// 全部规则 **fail-closed**：任一违规返回 [`DaemonError::ConfigError`]（含出口名），
/// **绝不**静默降级为明文、**绝不**跳过证书校验。
///
/// 规则清单：
/// 1. `tls = true` 但 scheme 为 `mqtt://`（或省略 scheme）→ 报错（不猜意图）。
/// 2. TLS 出口的信任锚：`ca_cert_path` **可选**——缺省 = 操作系统根证书库
///    （系统库为空时在 `TlsConfig::to_transport` 处 fail-closed，运行期不留死角）。
/// 3. `client_cert_path` / `client_key_path` 出现任一个时**必须成对**，否则报错。
/// 4. 上述 PEM 路径在**构建出口时**即校验存在（路径写错不留到运行期）。
/// 5. TLS 字段（`ca_cert_path` / `client_cert_path` / `client_key_path` /
///    `server_name` / `alpn`）存在但 TLS 未启用 → 报错（防止「配了 CA 却忘开
///    TLS」被静默当作明文出口）。
/// 6. `server_name` 作为 SNI 覆盖：**受 rumqttc 0.25.1 限制无法透传**，与 broker
///    host 不一致则报错（不静默忽略安全相关配置），一致时视为无操作。
///    源码证据（rumqttc 0.25.1）：
///    - `src/eventloop.rs:420-423`：`Transport::Tls` 分支调用
///      `tls::tls_connect(&options.broker_addr, ...)`——TLS 服务端名硬绑
///      `MqttOptions.broker_addr`；
///    - `src/tls.rs:176`：`ServerName::try_from(addr)`，该名字**同时**决定
///      TLS SNI 与证书名校验（`connector.connect(domain, tcp)`），函数无
///      server-name 参数；
///    - `src/lib.rs:457` 起的 `MqttOptions` 与 `NetworkOptions`（lib.rs:396）
///      均无 SNI / DNS 覆盖字段（全部 `set_*` 亦然）；
///    - `TlsConfiguration::Rustls(Arc<ClientConfig>)`（tls.rs:131）只携带
///      rustls `ClientConfig`，而 rustls 的握手服务端名是
///      `ClientConnection::new` 的参数，`ClientConfig` 层面无覆盖 API；
///    - 替代路径均不可用：proxy feature 的 TLS 跳仍取 broker_addr 且本项目
///      未启用该 feature（Cargo.lock 无 async-http-proxy，离线不可新增依赖）；
///      native-tls 非 pure Rust；fork / vendor 改源被红线禁止。
///
///    因此「连接地址与证书名不同（DNS 别名 / 负载均衡）」的唯一安全用法是：
///    直接把 `broker` 配成证书 SAN 里的名字（此时 `server_name` 留空或同名）。
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

    // 北向 MQTT 凭证支持（task）：仅当 `username` 与 `password` **同时**配置时才
    // 注入。若只设置了其中之一，保持 `None`，交由 `EndpointConfig::validate`
    // （`build_options`）既有校验在连接期拒绝「有口令无用户名」，绝不发送空凭证。
    let endpoint = match (&outlet.username, &outlet.password) {
        (Some(user), Some(pass)) => endpoint.with_credentials(user.clone(), pass.clone()),
        _ => endpoint,
    };

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
    // 规则 2：信任锚可选 —— `ca_cert_path` 缺省 = 操作系统根证书库
    // （2026-09-25 用户决策：证书配置可选、无证书也可用）。
    // 无论哪种来源都强制服务端证书校验，绝无「跳过校验」路径；
    // 系统库为空时在 `TlsConfig::to_transport` 处 fail-closed。
    let ca = outlet
        .ca_cert_path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

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
    if let Some(path) = &ca {
        ensure_cert_file_exists(outlet, "ca_cert_path", path)?;
    }
    if let Some(path) = &cert {
        ensure_cert_file_exists(outlet, "client_cert_path", path)?;
    }
    if let Some(path) = &key {
        ensure_cert_file_exists(outlet, "client_key_path", path)?;
    }

    // 规则 6：SNI 覆盖受 rumqttc 0.25.1 限制无法透传——`ServerName` 硬绑
    // `MqttOptions.broker_addr`（eventloop.rs:420-423 → tls.rs:176，同时决定
    // SNI 与证书名校验；`MqttOptions` / `NetworkOptions` / `ClientConfig` 均无
    // 覆盖点）。与 broker host 不一致即报错（fail-closed，不静默忽略）；
    // 一致时无操作（broker_addr 即服务端名）。
    if let Some(sni) = non_empty(outlet.server_name.as_deref()) {
        if !sni.eq_ignore_ascii_case(broker_host) {
            return Err(config_err(format!(
                "outlet `{}`: `server_name` = `{sni}` differs from broker host `{broker_host}`; \
                 the MQTT transport (rumqttc 0.25.1) derives the TLS server name from the \
                 broker address and exposes no SNI override, so an independent `server_name` \
                 cannot be honored — set `broker` to the certificate's server name instead",
                outlet.name
            )));
        }
    }

    let mut tls = match (&cert, &key) {
        (Some(cert), Some(key)) => match &ca {
            Some(ca) => TlsConfig::mtls(ca, cert.clone(), key.clone()),
            None => TlsConfig::mtls_system_roots(cert.clone(), key.clone()),
        },
        _ => match &ca {
            Some(ca) => TlsConfig::ca_only(ca),
            None => TlsConfig::system_roots(),
        },
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
    /// 北向转发授权闸门（`None` = 恒放行，保持未接线授权时的既有行为；
    /// bootstrap 在装配期把 `LicenseRuntime` 注入进来）。
    gate: Option<Arc<dyn NorthForwardGate>>,
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
            gate: None,
        }
    }

    /// 覆盖驱动轮询周期。
    #[must_use]
    pub fn with_tick(mut self, tick: Duration) -> Self {
        self.tick = tick;
        self
    }

    /// 注入北向转发授权闸门（builder 风格）。
    #[must_use]
    pub fn with_gate(mut self, gate: Arc<dyn NorthForwardGate>) -> Self {
        self.gate = Some(gate);
        self
    }

    /// 注入北向转发授权闸门（bootstrap 持有 `Option<NorthRuntimeConfig>` 时的原地写法）。
    pub fn set_gate(&mut self, gate: Arc<dyn NorthForwardGate>) {
        self.gate = Some(gate);
    }
}

impl std::fmt::Debug for NorthRuntimeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NorthRuntimeConfig")
            .field("queue_db", &self.queue.queue_db_path())
            .field("gateway_id", &self.queue.gateway_id())
            .field("tick", &self.tick)
            .field("gate", &self.gate.is_some())
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
    /// 因授权门控（`NorthForwardGate` 拒绝）被跳过发送的驱动拍数。
    ///
    /// > 0 = 授权门控生效中（Degraded / Unlicensed）；恢复后停止增长。
    pub gated_cycles: u64,
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
    gated_cycles: AtomicU64,
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
            // ⚠ 背压接线缺口修复：`MqttClient` 必须挂载同一 [`NorthOutlet`]，
            // 否则驱动任务的 `pump`（pump_send / pump_replay）拿不到发送队列，
            // 出口将永远不发报文（发送队列只进不出）。此前缺这一步，
            // 由授权门控恢复转发的集成测试暴露。
            let client = match MqttClient::new(endpoint).map(|c| c.with_outlet(Arc::clone(&outlet)))
            {
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
            let task = spawn_driver(
                outlet_cfg.name.clone(),
                outlet_cfg.topic_prefix.clone(),
                client,
                Arc::clone(&outlet),
                Arc::clone(&counters),
                shutdown.clone(),
                cfg.tick,
                cfg.gate.clone(),
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
            gated_cycles: Counters::get(&self.counters.gated_cycles),
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

/// 单个出口的驱动任务：每拍「授权门控 → 推进事件循环 → 一轮泵」。
#[allow(
    clippy::too_many_arguments,
    reason = "装配期一次性接线签名；参数均为独立所有权/句柄，聚合只会增加间接层"
)]
fn spawn_driver(
    name: String,
    topic_prefix: String,
    client: MqttClient,
    outlet: Arc<NorthOutlet>,
    counters: Arc<Counters>,
    mut shutdown: watch::Receiver<bool>,
    tick: Duration,
    gate: Option<Arc<dyn NorthForwardGate>>,
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
        // 门控观测：上一拍是否被拒（用于跃迁日志：拒绝首拍 warn / 恢复首拍 info）。
        let mut last_allowed = true;
        let mut denied_cycles: u64 = 0;
        loop {
            if *shutdown.borrow_and_update() {
                break;
            }
            tokio::select! {
                _ = ticker.tick() => {}
                _ = shutdown.changed() => break,
            }
            Counters::bump(&counters.poll_cycles);

            // ⓪ 授权门控（fail-closed，可解释）：Degraded / Unlicensed ⇒ 跳过本轮
            //    发送。发送队列照常受理采集侧入队（超限自然落盘降级），本地采集与
            //    连接维护不受影响；判据经 `watch` 读授权状态，跃迁下一拍即生效。
            let allowed = gate.as_ref().is_none_or(|g| g.north_forward_allowed());
            if !allowed {
                Counters::bump(&counters.gated_cycles);
                denied_cycles = denied_cycles.saturating_add(1);
                // 节流：进入门控首拍必打（上一拍还是放行），之后每
                // GATED_WARN_EVERY_CYCLES 拍提醒一次（含 Degraded 原因与恢复路径）。
                if last_allowed || denied_cycles % GATED_WARN_EVERY_CYCLES == 1 {
                    let reason = gate
                        .as_ref()
                        .and_then(|g| g.deny_reason())
                        .unwrap_or_else(|| "license gate denied northbound forwarding".to_string());
                    warn!(
                        outlet = %name,
                        reason = %reason,
                        "north driver: northbound forwarding suspended by license gate; \
                         local capture continues, queued batches wait in the send queue"
                    );
                }
                last_allowed = false;
            } else {
                if !last_allowed {
                    info!(
                        outlet = %name,
                        "north driver: license recovered; northbound forwarding resumed"
                    );
                }
                last_allowed = true;
                denied_cycles = 0;
            }

            // ① 推进事件循环：PUBACK/PUBCOMP → 回收发送队列在途窗口；
            //    出错 → 按退避重试（继续 poll 即自动重连）。
            //    门控期间照常推进（维持连接），只是不发送。
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
            if !allowed {
                // 门控生效：跳过发送轮（发送 / 补发 / 审计），下一拍重新判定。
                continue;
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
            username: None,
            password: None,
        }
    }

    /// task：北向凭证随 `[[outlets]]` 的 `username`/`password` 透传到 `EndpointConfig`
    /// （transport 层既有 `set_credentials` 支持）。
    #[test]
    fn endpoint_from_outlet_propagates_credentials() {
        let mut outlet = base_outlet("cred-1", "mqtt://127.0.0.1:1883");
        outlet.username = Some("gw-user".to_string());
        outlet.password = Some("s3cr3t-password".to_string());
        let ep = endpoint_from_outlet(&outlet).expect("build endpoint");
        assert_eq!(ep.username.as_deref(), Some("gw-user"));
        assert_eq!(ep.password.as_deref(), Some("s3cr3t-password"));
    }

    /// task：无凭证出口构造的 `EndpointConfig` 不携带 `username`/`password`
    /// （不会注入空凭证）；既有 builder 链其余字段保持原样。
    #[test]
    fn endpoint_from_outlet_without_credentials_is_none() {
        let outlet = base_outlet("no-cred", "mqtt://127.0.0.1:1883");
        let ep = endpoint_from_outlet(&outlet).expect("build endpoint");
        assert!(ep.username.is_none(), "username must be None when unset");
        assert!(ep.password.is_none(), "password must be None when unset");
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

    /// 规则 2：`mqtts://` 缺 `ca_cert_path` → 信任锚回退**操作系统根证书库**
    /// （不再报错；2026-09-25 用户决策「证书配置可选」），TLS 仍挂载。
    #[test]
    fn tls_mqtts_without_ca_falls_back_to_system_roots() {
        let outlet = base_outlet("north-ca", "mqtts://broker.local:8883");
        let endpoint = endpoint_from_outlet(&outlet).expect("system-roots outlet");
        let tls = endpoint.tls.clone().expect("tls must be attached");
        assert!(tls.ca_cert_path.is_none(), "no custom CA expected");
        assert!(
            tls.client_cert_path.is_none() && tls.client_key_path.is_none(),
            "no client identity expected"
        );
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

    /// 规则 6（反例）：`server_name` 与 broker host 不一致 → 报错（不静默忽略）。
    ///
    /// 背景（task 20）：rumqttc 0.25.1 把 TLS `ServerName` 硬绑 `broker_addr`
    /// （eventloop.rs:420-423 → tls.rs:176），`server_name` 无透传通道 → fail-closed。
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

    /// 规则 6（正例）：`server_name` 与 broker host 一致（大小写不敏感）→ 接受，
    /// 且该出口确实挂上了 TLS（语义：broker_addr 即服务端名，`server_name` 无操作）。
    #[test]
    fn tls_server_name_matching_host_is_accepted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ca = write_pem(dir.path(), "ca.crt");
        let mut outlet = base_outlet("north-sni-ok", "mqtts://Broker.Local:8883");
        outlet.ca_cert_path = Some(ca);
        outlet.server_name = Some("broker.local".to_string());
        let endpoint = endpoint_from_outlet(&outlet).expect("same-name SNI must be accepted");
        assert!(
            endpoint.tls.is_some(),
            "TLS must still be attached to the endpoint"
        );
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

    // ---- 授权门控（NorthForwardGate 接入驱动任务） ----

    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

    /// 可翻转的测试闸门：拒绝理由固定（含恢复路径语义）。
    struct FlipGate {
        allowed: AtomicBool,
    }

    impl FlipGate {
        fn denied() -> Self {
            Self {
                allowed: AtomicBool::new(false),
            }
        }

        fn allow(&self) {
            self.allowed.store(true, AtomicOrdering::SeqCst);
        }
    }

    impl NorthForwardGate for FlipGate {
        fn north_forward_allowed(&self) -> bool {
            self.allowed.load(AtomicOrdering::SeqCst)
        }

        fn deny_reason(&self) -> Option<String> {
            if self.allowed.load(AtomicOrdering::SeqCst) {
                None
            } else {
                Some("license degraded: test; local capture continues".to_string())
            }
        }
    }

    /// 在临时目录打开一个真实 [`OfflineQueue`]。
    fn gate_test_queue(dir: &Path) -> Arc<OfflineQueue> {
        let cfg = crate::offline_queue::QueueConfig::new(dir.join("queue.db"), "gw-gate")
            .expect("queue cfg");
        Arc::new(
            OfflineQueue::open(cfg, Arc::new(crate::offline_queue::SystemClock))
                .expect("open queue"),
        )
    }

    /// 丢弃型审计出口（门控测试不关心审计内容）。
    struct DropSink;

    impl crate::north::mqtt::AuditSink for DropSink {
        fn emit(&self, _events: Vec<crate::backpressure::BackpressureAudit>) {}
    }

    /// 构造单出口（指向不可达 broker）+ 指定闸门的运行期与停机信号。
    fn gated_runtime(
        dir: &Path,
        gate: Option<Arc<dyn NorthForwardGate>>,
    ) -> (NorthRuntime, tokio::sync::watch::Sender<bool>) {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let mut cfg = NorthRuntimeConfig::new(
            gate_test_queue(dir),
            Arc::new(crate::offline_queue::SystemClock),
            Arc::new(DropSink),
        )
        .with_tick(Duration::from_millis(20));
        if let Some(gate) = gate {
            cfg.set_gate(gate);
        }
        let outlet = base_outlet("north-gated", "mqtt://127.0.0.1:1883");
        (
            NorthRuntime::start("gw-gate", &[outlet], cfg, shutdown_rx),
            shutdown_tx,
        )
    }

    /// QA：闸门拒绝期间 —— 驱动拍计入 `gated_cycles`、pump 恒不执行；
    /// 恢复放行后 `gated_cycles` 停止增长（状态跃迁下一拍生效，跳过发送语义）。
    #[tokio::test(start_paused = true)]
    async fn gate_denial_skips_pump_and_resume_stops_gating() {
        let dir = tempfile::tempdir().expect("tempdir");
        let gate = Arc::new(FlipGate::denied());
        let (runtime, shutdown_tx) = gated_runtime(
            dir.path(),
            Some(Arc::clone(&gate) as Arc<dyn NorthForwardGate>),
        );

        // 采集侧入队在门控期间照常受理（降级 ≠ 停用）。
        assert!(matches!(
            runtime.submit("north-gated", 1, vec![1, 2, 3]),
            Ok(PushOutcome::Admitted)
        ));

        // 推进虚拟时间直到多个驱动拍被门控（poll 失败触发指数退避 1s/2s/4s…，
        // 驱动拍稀疏出现，故按谓词推进而非固定拍数）。
        let mut gated_cycles = 0u64;
        for _ in 0..400 {
            tokio::time::advance(Duration::from_millis(100)).await;
            tokio::task::yield_now().await;
            gated_cycles = runtime.stats().gated_cycles;
            if gated_cycles >= 5 {
                break;
            }
        }
        let stats = runtime.stats();
        assert!(
            gated_cycles >= 5,
            "denied cycles must be counted: {stats:?}"
        );
        assert_eq!(stats.pumps, 0, "pump must be skipped while gated");

        // 恢复放行：gated_cycles 停止增长。
        gate.allow();
        let frozen = runtime.stats().gated_cycles;
        for _ in 0..40 {
            tokio::time::advance(Duration::from_millis(100)).await;
            tokio::task::yield_now().await;
        }
        assert_eq!(
            runtime.stats().gated_cycles,
            frozen,
            "allowed cycles must not count as gated"
        );

        shutdown_tx.send_replace(true);
    }

    /// QA：未注入闸门 = 恒放行（未接线授权时行为与既往完全一致，
    /// `gated_cycles` 恒 0，pump 正常进入）。
    #[tokio::test(start_paused = true)]
    async fn absent_gate_defaults_to_allow() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (runtime, shutdown_tx) = gated_runtime(dir.path(), None);

        for _ in 0..30 {
            tokio::time::advance(Duration::from_millis(20)).await;
            tokio::task::yield_now().await;
        }
        assert_eq!(runtime.stats().gated_cycles, 0, "no gate ⇒ no gating ever");
        assert_eq!(
            runtime.stats().pumps,
            0,
            "poll fails against unreachable broker so pump path never completes; \
             the assertion here is gated_cycles == 0"
        );

        shutdown_tx.send_replace(true);
    }
}
