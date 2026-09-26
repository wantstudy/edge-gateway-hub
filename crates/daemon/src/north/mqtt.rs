//! 北向 MQTT 客户端（计划 task 19）：rumqttc（纯 Rust + rustls）单客户端 + 多路连接池。
//!
//! ## 职责边界
//! - **做**：单路 [`MqttClient`] 的连接配置 / 选项构建 / 发布订阅 / 事件循环推进 /
//!   自动重连与会话恢复 / 退避策略；多路连接按名管理的 [`MqttConnectionPool`]；
//!   **每路连接独立声明 `encoding`**（[`Encoding::Protobuf`] 默认 / [`Encoding::Json`]）。
//! - **做（task 54 接线）**：北向出站的**背压接线**——有界发送队列（水位 + 落盘降级）、
//!   **PUBACK 门控**确认回收、补发路径的批次级幂等去重、审计的取走上报出口；
//!   见 [`NorthOutlet`]。
//! - **不做**：载荷的 protobuf / JSON 序列化（= task 62，本模块只做配置读取与传递，
//!   见 [`PayloadEncoder`] 契约）；转发规则与触发时机（= task 20/37）；
//!   离线缓存与补发（= task 17/18）。
//!
//! ## 关键设计决策
//!
//! 1. **编码只传递不实现**：`encoding` 是连接的**声明式属性**（存在 [`EndpointConfig`] 里，
//!    随连接一起传给上游），本模块不做任何序列化。`publish_sample` 要求调用方注入
//!    [`PayloadEncoder`]（task 62 实现），并**校验编码器声明的 encoding 与本路连接一致**，
//!    不一致即 [`DaemonError::ConfigError`]（避免 protobuf 字节被当成 JSON 主题发出去）。
//! 2. **自动重连靠「继续 poll」**：rumqttc 的语义是——`EventLoop::poll()` 出错后
//!    `network` 置空并把未确认报文搬进 `pending`，下一次 `poll()` 自动重连。
//!    因此本模块**不吞掉错误**：`poll_event()` 把错误返回给调用方（驱动循环），
//!    同时记录退避间隔（[`MqttClient::backoff`]），由调用方决定何时继续推进。
//! 3. **会话恢复以 CONNACK 的 `session_present` 为准**：`clean_session = false` +
//!    固定 `client_id` 时，重连后 broker 回 `session_present = true`，rumqttc 据此
//!    **不清空 `pending`**，未确认的 QoS>0 报文自动重发（MQTT 3.1.1 语义）。
//!    这是「会话恢复」唯一可信的观测点，故由 `poll_event()` 捕获并暴露。
//! 4. **配置错误在构建期暴露**：空 broker / 空 client_id / port 0 / 非法 QoS /
//!    keep_alive 小于 1s / 半套 mTLS / 主题前缀含通配符 → 一律
//!    [`DaemonError::ConfigError`]（2000），不把 panic 留给 rumqttc 的 `assert!`。
//! 5. **错误域收敛**：连接关（TLS/证书）→ [`DaemonError::SecurityError`]（7000）；
//!    传输关（IO/超时）→ [`DaemonError::NetworkError`]（6000）；协议关（状态机/被拒）→
//!    [`DaemonError::MqttError`]（5000）。见 [`map_connection_error`]。
//! 6. **重连退避复用南向 [`crate::driver::Reconnector`]**：纯逻辑无 IO，语义一致，
//!    避免北向再造一套退避（默认值 1s → 60s、倍率 2，可按 endpoint 调）。
//! 7. **背压接线（task 54，接入 [`crate::backpressure`] 原语，本模块只编排不改原语）**：
//!    - [`NorthSendQueue`]：`push` → [`MqttClient::pump_send`]（`take_ready` → 发布）→
//!      **PUBACK 到达才** `confirm(1)`（见 [`MqttClient::poll_event`]）→ 发布失败
//!      `requeue_failed` 回灌 `ready` 头部（水位占用不变，不丢数据）。
//!    - [`NorthReplay`]：`should_send` → 发布 → `ack`；**「先落 Ack 后推位点」的顺序由
//!      [`ReplayController::ack`] 内部保证，本模块不得颠倒**（颠倒了会丢数据）。
//!    - [`IngestGate`]：消费端 / 出口入库前用 `IdempotencyLedger::apply_key` 判定，
//!      仅 `Applied` 才入库（`Duplicate` 跳过 = 计划允许的唯一隐式丢弃）。
//!    - [`AuditPump`]：`AuditLog::drain()` 的调用点（有界 4096，`dropped()` 可观测
//!      「审计消费变慢」）。
//!    - **慢消费者硬红线**：`push` 只做一次水位比较 + 一次有界落盘调用，**绝不阻塞采集路径**。

use std::collections::VecDeque;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

// task 61：依赖包名已从 `rumqttc` 换成 `rumqttc-v4-next`（rumqttc-next 家族的 MQTT 3.1.1
// 变体，用于清掉 rustls-webpki 0.102.x 的 4 条公告），但该 crate 的 `[lib] name` 仍是
// `rumqttc`，故此处 import 路径不变——不是漏改。见 crates/daemon/Cargo.toml 的迁移说明。
use rumqttc::{
    AsyncClient, ConnectReturnCode, ConnectionError, Event, EventLoop, MqttOptions, Packet, QoS,
    TlsConfiguration, Transport,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{crypto::CryptoProvider, ClientConfig, RootCertStore};

use crate::backpressure::{
    AckOutcome, AuditLog, BackpressureAudit, DedupOutcome, IdempotencyLedger, LedgerStats,
    PendingSend, PushOutcome, QueueAckSink, QueueSpillSink, ReplayController, SendQueue,
    SendQueueStats, WaterLevel,
};
use crate::driver::Reconnector;
use crate::error::{DaemonError, DaemonResult};
use crate::offline_queue::{Clock, OfflineQueue, QueuedBatch, DEFAULT_MEM_HIGH_WATER_ROWS};
use crate::pipeline::ProcessedSample;

// ---- 常量（默认值集中声明，避免散落魔法数字） ----

/// 明文 MQTT 默认端口。
pub const DEFAULT_MQTT_PORT: u16 = 1883;
/// TLS MQTT 默认端口。
pub const DEFAULT_MQTTS_PORT: u16 = 8883;
/// 默认 keep alive（rumqttc 默认 60s，语义：空闲时发 PINGREQ 的间隔）。
pub const DEFAULT_KEEP_ALIVE: Duration = Duration::from_secs(60);
/// 默认客户端标识前缀（`<prefix>-<endpoint名>`）。
pub const CLIENT_ID_PREFIX: &str = "iot-daq";
/// 默认主题前缀（与 `config.rs` 的 `default_topic_prefix` 对齐）。
pub const DEFAULT_TOPIC_PREFIX: &str = "telemetry";
/// 默认请求通道容量（背压：通道满时 `publish` 等待事件循环消费）。
pub const DEFAULT_REQUEST_CHANNEL_CAPACITY: usize = 64;
/// 默认慢消费者发送水位（未确认积压阈值；task 54。必须 < 请求通道容量，
/// 使水位降级先于通道阻塞生效）。
pub const DEFAULT_SEND_HIGH_WATER: usize = 32;
/// 默认最大并发 inflight 报文数。
pub const DEFAULT_INFLIGHT: u16 = 100;
/// 默认重连初始退避。
pub const DEFAULT_RECONNECT_INITIAL: Duration = Duration::from_secs(1);
/// 默认重连退避上限。
pub const DEFAULT_RECONNECT_MAX: Duration = Duration::from_secs(60);
/// 默认重连退避倍率。
pub const DEFAULT_RECONNECT_MULTIPLIER: u32 = 2;

// ---- 编码声明（task 19 只声明与传递；序列化 = task 62） ----

/// 北向载荷编码（每路连接独立声明，计划决议：protobuf 默认 / json 可选）。
///
/// 本枚举**只表达声明**，不含任何序列化行为：序列化由 task 62 的 [`PayloadEncoder`]
/// 实现提供，本模块负责把声明从配置带到位并做一致性校验。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Encoding {
    /// Protobuf 编码（默认）。
    #[default]
    Protobuf,
    /// JSON 编码（int64 → string 约定见 `protocol-proto` 头注释）。
    Json,
}

impl Encoding {
    /// 配置字面量（小写，与 TOML `encoding = "json"` 一致）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Encoding::Protobuf => "protobuf",
            Encoding::Json => "json",
        }
    }

    /// 从配置字面量解析（大小写不敏感；`proto` 视为 `protobuf` 别名）。
    ///
    /// # Errors
    /// 未知字面量返回 [`DaemonError::ConfigError`]（错误码 2000）。
    pub fn parse(raw: &str) -> DaemonResult<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "protobuf" | "proto" => Ok(Encoding::Protobuf),
            "json" => Ok(Encoding::Json),
            other => Err(DaemonError::ConfigError(format!(
                "outlet encoding: unknown encoding `{other}` (expected `protobuf` or `json`)"
            ))),
        }
    }
}

impl fmt::Display for Encoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<crate::config::OutletEncoding> for Encoding {
    /// 配置层 `[[outlets]].encoding` → 北向连接编码声明（单一转换点）。
    fn from(value: crate::config::OutletEncoding) -> Self {
        match value {
            crate::config::OutletEncoding::Protobuf => Encoding::Protobuf,
            crate::config::OutletEncoding::Json => Encoding::Json,
        }
    }
}

// ---- QoS 辅助 ----

/// QoS 等级 → rumqttc [`QoS`]。
///
/// # Errors
/// `level > 2` 返回 [`DaemonError::ConfigError`]。
pub fn qos_from_u8(level: u8) -> DaemonResult<QoS> {
    match level {
        0 => Ok(QoS::AtMostOnce),
        1 => Ok(QoS::AtLeastOnce),
        2 => Ok(QoS::ExactlyOnce),
        other => Err(DaemonError::ConfigError(format!(
            "outlet qos: {other} out of range (expected 0/1/2)"
        ))),
    }
}

/// rumqttc [`QoS`] → 配置等级（0/1/2）。
pub fn qos_to_u8(qos: QoS) -> u8 {
    match qos {
        QoS::AtMostOnce => 0,
        QoS::AtLeastOnce => 1,
        QoS::ExactlyOnce => 2,
    }
}

// ---- TLS / mTLS ----

/// TLS 配置（PEM **路径**，内容在构建 [`Transport`] 时读取，不入库、不硬编码）。
///
/// - `ca_cert_path`：服务端 CA；**可选**——`None` = 用**操作系统根证书库**校验
///   broker 证书（2026-09-25 用户决策：证书配置可选，没有证书文件也可用）。
///   无论哪种来源都**强制**服务端证书校验，**无「跳过校验」路径**；
/// - `client_cert_path` + `client_key_path`：成对出现即启用 mTLS。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TlsConfig {
    /// CA 证书 PEM 路径（可选；`None` = 操作系统根证书库）。
    pub ca_cert_path: Option<PathBuf>,
    /// 客户端证书 PEM 路径（mTLS；与 `client_key_path` 成对）。
    pub client_cert_path: Option<PathBuf>,
    /// 客户端私钥 PEM 路径（mTLS；与 `client_cert_path` 成对）。
    pub client_key_path: Option<PathBuf>,
    /// ALPN 协议列表（如 `b"mqtt"`；空 = 不协商）。
    pub alpn: Vec<Vec<u8>>,
}

impl TlsConfig {
    /// 仅服务端校验（无 mTLS），信任锚 = 指定 CA 文件。
    pub fn ca_only(ca_cert_path: impl Into<PathBuf>) -> Self {
        Self {
            ca_cert_path: Some(ca_cert_path.into()),
            client_cert_path: None,
            client_key_path: None,
            alpn: Vec::new(),
        }
    }

    /// 仅服务端校验（无 mTLS），信任锚 = **操作系统根证书库**（无自定义 CA 文件）。
    pub fn system_roots() -> Self {
        Self {
            ca_cert_path: None,
            client_cert_path: None,
            client_key_path: None,
            alpn: Vec::new(),
        }
    }

    /// mTLS（服务端校验 + 客户端证书），信任锚 = 指定 CA 文件。
    pub fn mtls(
        ca_cert_path: impl Into<PathBuf>,
        client_cert_path: impl Into<PathBuf>,
        client_key_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            ca_cert_path: Some(ca_cert_path.into()),
            client_cert_path: Some(client_cert_path.into()),
            client_key_path: Some(client_key_path.into()),
            alpn: Vec::new(),
        }
    }

    /// mTLS（服务端校验 + 客户端证书），信任锚 = **操作系统根证书库**。
    pub fn mtls_system_roots(
        client_cert_path: impl Into<PathBuf>,
        client_key_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            ca_cert_path: None,
            client_cert_path: Some(client_cert_path.into()),
            client_key_path: Some(client_key_path.into()),
            alpn: Vec::new(),
        }
    }

    /// 配置自检：mTLS 证书与私钥必须成对。
    ///
    /// `ca_cert_path` **可选**（`None` = 操作系统根证书库，见 [`TlsConfig::system_roots`]）。
    ///
    /// # Errors
    /// 非法组合返回 [`DaemonError::ConfigError`]（2000）。
    pub fn validate(&self) -> DaemonResult<()> {
        match (&self.client_cert_path, &self.client_key_path) {
            (Some(_), None) => Err(DaemonError::ConfigError(
                "outlet tls: client_key_path is required when client_cert_path is set".to_string(),
            )),
            (None, Some(_)) => Err(DaemonError::ConfigError(
                "outlet tls: client_cert_path is required when client_key_path is set".to_string(),
            )),
            _ => Ok(()),
        }
    }

    /// 构造 rumqttc [`Transport`]（rustls，注入急切构建的 [`ClientConfig`]）。
    ///
    /// 信任锚来源：`ca_cert_path` 提供则读该 PEM（可含证书链）；缺省则加载
    /// **操作系统根证书库**（`rustls-native-certs`）。两者皆**强制**服务端证书
    /// 校验——信任锚解析不到任何证书即 [`DaemonError::SecurityError`]，
    /// **无「跳过校验」路径**。本方法会先确保进程级 `CryptoProvider` 就位
    /// （见 [`ensure_rustls_provider`]），因此**宿主二进制无需额外调用
    /// `install_default()`**。
    ///
    /// 相比「把 PEM 字节交给 rumqttc 建连时惰性解析」的旧实现，这里**急切**构建
    /// `ClientConfig`：证书/私钥非法在出口启动时即暴露（fail-fast），不留到首次握手。
    ///
    /// # Errors
    /// 文件读取/内容非法或信任锚为空 → [`DaemonError::SecurityError`]（7000）；
    /// 配置非法 → [`DaemonError::ConfigError`]（2000）。
    pub fn to_transport(&self) -> DaemonResult<Transport> {
        self.validate()?;
        ensure_rustls_provider()?;

        let roots = self.build_root_store()?;
        let builder = ClientConfig::builder().with_root_certificates(roots);
        let mut config = match (&self.client_cert_path, &self.client_key_path) {
            (Some(cert), Some(key)) => {
                let (chain, key) = load_client_identity(cert, key)?;
                builder.with_client_auth_cert(chain, key).map_err(|e| {
                    DaemonError::SecurityError(format!(
                        "outlet tls: invalid client identity (cert/key mismatch?): {e}"
                    ))
                })?
            }
            _ => builder.with_no_client_auth(),
        };
        config.alpn_protocols.clone_from(&self.alpn);

        Ok(Transport::Tls(TlsConfiguration::Rustls(Arc::new(config))))
    }

    /// 构建服务端信任锚：`ca_cert_path` 有值用该文件，缺省用**操作系统根证书库**。
    ///
    /// fail-closed：两条路径都解析不到任何证书 → [`DaemonError::SecurityError`]。
    fn build_root_store(&self) -> DaemonResult<RootCertStore> {
        let mut roots = RootCertStore::empty();
        match self.ca_cert_path.as_deref() {
            Some(path) => {
                let pem = read_pem(path, "ca_cert_path")?;
                if load_certs_into(&mut roots, &pem, "ca_cert_path")? == 0 {
                    return Err(DaemonError::SecurityError(format!(
                        "outlet tls: `ca_cert_path` `{}` contains no certificate",
                        path.display()
                    )));
                }
            }
            None => {
                let loaded = rustls_native_certs::load_native_certs();
                if !loaded.errors.is_empty() {
                    return Err(DaemonError::SecurityError(format!(
                        "outlet tls: failed to load OS root certificate store: {:?}",
                        loaded.errors
                    )));
                }
                for cert in loaded.certs {
                    roots.add(cert).map_err(|e| {
                        DaemonError::SecurityError(format!(
                            "outlet tls: OS root store contains an unparsable certificate: {e}"
                        ))
                    })?;
                }
                if roots.is_empty() {
                    return Err(DaemonError::SecurityError(
                        "outlet tls: OS root certificate store is empty; provide \
                         `ca_cert_path` explicitly to verify the broker certificate \
                         (skipping certificate verification is not supported)"
                            .to_string(),
                    ));
                }
            }
        }
        Ok(roots)
    }
}

/// 读取 PEM 文件并做基本内容校验（非空 + 含 `BEGIN` 标记）。
fn read_pem(path: &Path, field: &str) -> DaemonResult<Vec<u8>> {
    let bytes = std::fs::read(path).map_err(|e| {
        DaemonError::SecurityError(format!(
            "outlet tls: read {field} `{}`: {e}",
            path.display()
        ))
    })?;
    if bytes.is_empty() || !bytes.windows(10).any(|w| w == b"-----BEGIN") {
        return Err(DaemonError::SecurityError(format!(
            "outlet tls: {field} `{}` is not a PEM file",
            path.display()
        )));
    }
    Ok(bytes)
}

/// 将 PEM 字节中的全部证书解析进 [`RootCertStore`]，返回解析数量。
fn load_certs_into(store: &mut RootCertStore, pem: &[u8], field: &str) -> DaemonResult<usize> {
    let mut count = 0usize;
    for cert in rustls_pemfile::certs(&mut std::io::Cursor::new(pem)) {
        let cert = cert
            .map_err(|e| DaemonError::SecurityError(format!("outlet tls: parse {field}: {e}")))?
            .into_owned();
        store.add(cert).map_err(|e| {
            DaemonError::SecurityError(format!(
                "outlet tls: unparsable certificate in {field}: {e}"
            ))
        })?;
        count += 1;
    }
    Ok(count)
}

/// 读取并解析 mTLS 客户端身份：证书链（全部证书）+ 私钥（取第一个可用项）。
fn load_client_identity(
    cert_path: &Path,
    key_path: &Path,
) -> DaemonResult<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let cert_pem = read_pem(cert_path, "client_cert_path")?;
    let mut chain = Vec::new();
    for cert in rustls_pemfile::certs(&mut std::io::Cursor::new(cert_pem.as_slice())) {
        let cert = cert
            .map_err(|e| {
                DaemonError::SecurityError(format!("outlet tls: parse client_cert_path: {e}"))
            })?
            .into_owned();
        chain.push(cert);
    }
    if chain.is_empty() {
        return Err(DaemonError::SecurityError(
            "outlet tls: `client_cert_path` contains no certificate".to_string(),
        ));
    }

    let key_pem = read_pem(key_path, "client_key_path")?;
    let key = rustls_pemfile::private_key(&mut std::io::Cursor::new(key_pem.as_slice()))
        .map_err(|e| DaemonError::SecurityError(format!("outlet tls: parse client_key_path: {e}")))?
        .ok_or_else(|| {
            DaemonError::SecurityError(
                "outlet tls: `client_key_path` contains no private key".to_string(),
            )
        })?
        .clone_key();
    Ok((chain, key))
}

/// 确保 rustls 有进程级 `CryptoProvider`（mqtts 握手的前提）。
///
/// 策略：**尊重宿主选择**——若宿主已安装 provider（自定义 / FIPS 等）则原样保留；
/// 否则安装 rustls 的 ring 默认 provider（幂等，进程内只能成功一次，竞态丢失属正常）。
///
/// 引用 `rustls::crypto::ring` 同时构成**编译期守护**：一旦有人在 `Cargo.toml` 里
/// 移除 rustls 的 `ring` provider 特性，本文件将无法编译（另有
/// `tls_crypto_provider_is_installed` 单测做运行时守护）。
///
/// # Errors
/// 安装失败且确实无 provider → [`DaemonError::SecurityError`]（7000）。
fn ensure_rustls_provider() -> DaemonResult<()> {
    if CryptoProvider::get_default().is_some() {
        return Ok(());
    }
    match rustls::crypto::ring::default_provider().install_default() {
        Ok(()) => Ok(()),
        // 已有人（宿主或并发）先装上了 → 只要确实存在即视为就绪。
        Err(_) if CryptoProvider::get_default().is_some() => Ok(()),
        Err(_) => Err(DaemonError::SecurityError(
            "outlet tls: failed to install a rustls CryptoProvider".to_string(),
        )),
    }
}

// ---- 单路连接配置 ----

/// 一路北向 MQTT 连接的完整配置。
///
/// 每路独立声明 `broker` / `port` / `client_id` / `qos` / `encoding` / `tls` /
/// `clean_session`，互不干扰（多 Broker 场景由 [`MqttConnectionPool`] 按名路由）。
///
/// **Debug 脱敏**：手写实现，`password` 恒打码为 `<redacted>`
/// —— 禁止把口令带进日志 / 错误串 / 崩溃转储。
#[derive(Clone)]
pub struct EndpointConfig {
    /// 出口名（连接池唯一键，日志与诊断用）。
    pub name: String,
    /// Broker 主机 / IP。
    pub broker: String,
    /// Broker 端口（1-65535）。
    pub port: u16,
    /// MQTT 客户端标识（会话恢复的关键：重连必须保持一致）。
    pub client_id: String,
    /// 用户名（可选）。
    pub username: Option<String>,
    /// 口令（可选；**从配置/env 注入，禁止硬编码**）。
    pub password: Option<String>,
    /// 主题前缀（`{prefix}/{device_id}/{point_id}`）。
    pub topic_prefix: String,
    /// 该路连接的默认 QoS。
    pub qos: QoS,
    /// 该路连接的载荷编码声明（默认 protobuf）。
    pub encoding: Encoding,
    /// `true` = 每次连接都是干净会话；`false` = 持久会话（可恢复）。
    pub clean_session: bool,
    /// Keep alive（rumqttc 约束：0 或 ≥ 1s）。
    pub keep_alive: Duration,
    /// TLS / mTLS 配置（`None` = 明文）。
    pub tls: Option<TlsConfig>,
    /// 请求通道容量（背压阈值）。
    pub request_channel_capacity: usize,
    /// 慢消费者发送水位（未确认积压阈值；task 54）。
    ///
    /// 积压达到该值时，[`MqttClient::publish_backpressured`] 不再提交到发送通道，
    /// 改为调用注入的 [`SlowConsumerSink`]（调用方在此走离线落盘降级）。
    /// 必须满足 `1 <= send_high_water <= request_channel_capacity`。
    pub send_high_water: usize,
    /// 最大并发 inflight 报文数。
    pub inflight: u16,
    /// 重连初始退避。
    pub reconnect_initial: Duration,
    /// 重连退避上限。
    pub reconnect_max: Duration,
    /// 重连退避倍率。
    pub reconnect_multiplier: u32,
}

impl EndpointConfig {
    /// 按出口名 / broker / 端口构造（其余取默认值：`client_id` = `iot-daq-<name>`）。
    pub fn new(name: impl Into<String>, broker: impl Into<String>, port: u16) -> Self {
        let name = name.into();
        Self {
            client_id: format!("{CLIENT_ID_PREFIX}-{name}"),
            name,
            broker: broker.into(),
            port,
            username: None,
            password: None,
            topic_prefix: DEFAULT_TOPIC_PREFIX.to_string(),
            qos: QoS::AtLeastOnce,
            encoding: Encoding::Protobuf,
            clean_session: true,
            keep_alive: DEFAULT_KEEP_ALIVE,
            tls: None,
            request_channel_capacity: DEFAULT_REQUEST_CHANNEL_CAPACITY,
            send_high_water: DEFAULT_SEND_HIGH_WATER,
            inflight: DEFAULT_INFLIGHT,
            reconnect_initial: DEFAULT_RECONNECT_INITIAL,
            reconnect_max: DEFAULT_RECONNECT_MAX,
            reconnect_multiplier: DEFAULT_RECONNECT_MULTIPLIER,
        }
    }

    /// 设置客户端标识（会话恢复必须保持跨重连一致）。
    pub fn with_client_id(mut self, client_id: impl Into<String>) -> Self {
        self.client_id = client_id.into();
        self
    }

    /// 设置用户名 / 口令（口令从配置或 env 注入，本模块不落盘、不硬编码）。
    pub fn with_credentials(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }

    /// 设置 TLS / mTLS。
    pub fn with_tls(mut self, tls: TlsConfig) -> Self {
        self.tls = Some(tls);
        self
    }

    /// 设置 QoS（`0`/`1`/`2`；越界返回 [`DaemonError::ConfigError`]）。
    pub fn with_qos(mut self, level: u8) -> DaemonResult<Self> {
        self.qos = qos_from_u8(level)?;
        Ok(self)
    }

    /// 设置该路连接的载荷编码声明。
    pub fn with_encoding(mut self, encoding: Encoding) -> Self {
        self.encoding = encoding;
        self
    }

    /// 设置会话模式（`false` = 持久会话，可会话恢复）。
    pub fn with_clean_session(mut self, clean_session: bool) -> Self {
        self.clean_session = clean_session;
        self
    }

    /// 设置 keep alive（rumqttc 约束：0 或 ≥ 1s）。
    pub fn with_keep_alive(mut self, keep_alive: Duration) -> Self {
        self.keep_alive = keep_alive;
        self
    }

    /// 设置主题前缀。
    pub fn with_topic_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.topic_prefix = prefix.into();
        self
    }

    /// 设置请求通道容量与慢消费者发送水位（`high` 必须落在 `1..=capacity`）。
    ///
    /// # Errors
    /// `high == 0` 或 `high > capacity` → [`DaemonError::ConfigError`]（validate 时拦截）。
    pub fn with_send_watermarks(mut self, capacity: usize, high: usize) -> Self {
        self.request_channel_capacity = capacity;
        self.send_high_water = high;
        self
    }

    /// 设置重连退避参数（初始 / 上限 / 倍率）。
    pub fn with_reconnect(mut self, initial: Duration, max: Duration, multiplier: u32) -> Self {
        self.reconnect_initial = initial;
        self.reconnect_max = max;
        self.reconnect_multiplier = multiplier;
        self
    }

    /// `broker:port` 文本（日志用，不含凭证）。
    pub fn address(&self) -> String {
        format!("{}:{}", self.broker, self.port)
    }

    /// 拼接点位主题：`{topic_prefix}/{device_id}/{point_id}`。
    pub fn topic_for(&self, device_id: &str, point_id: &str) -> String {
        format!("{}/{}/{}", self.topic_prefix, device_id, point_id)
    }

    /// 配置自检（构建期；非法即 [`DaemonError::ConfigError`]，错误码 2000）。
    ///
    /// 逐条校验：出口名 / broker / 端口 / client_id / keep_alive / 凭证配对 /
    /// 主题前缀通配符 / TLS 组合。
    ///
    /// # Errors
    /// 任一字段非法返回 [`DaemonError::ConfigError`]。
    pub fn validate(&self) -> DaemonResult<()> {
        let bad = |field: &str, why: &str| {
            DaemonError::ConfigError(format!("outlet `{}`: {field} {why}", self.name))
        };

        if self.name.trim().is_empty() {
            return Err(bad("name", "must not be empty"));
        }
        if self.broker.trim().is_empty() {
            return Err(bad("broker", "must not be empty"));
        }
        if self.port == 0 {
            return Err(bad("port", "must be in 1..=65535 (0 is invalid)"));
        }
        if self.client_id.is_empty() {
            return Err(bad(
                "client_id",
                "must not be empty (required for persistent sessions)",
            ));
        }
        // rumqttc 的 set_keep_alive 对 (0, 1s) 区间直接 assert!，此处前置拦截。
        if !self.keep_alive.is_zero() && self.keep_alive < Duration::from_secs(1) {
            return Err(bad(
                "keep_alive",
                "must be zero or at least 1s (rumqttc constraint)",
            ));
        }
        if self.password.is_some() && self.username.is_none() {
            return Err(bad("credentials", "password without username is invalid"));
        }
        if self.topic_prefix.is_empty()
            || self.topic_prefix.contains('+')
            || self.topic_prefix.contains('#')
        {
            return Err(bad(
                "topic_prefix",
                "must be non-empty and free of MQTT wildcards (`+` / `#`)",
            ));
        }
        if self.request_channel_capacity == 0 {
            return Err(bad("request_channel_capacity", "must be at least 1"));
        }
        if self.send_high_water == 0 {
            return Err(bad("send_high_water", "must be at least 1"));
        }
        if self.send_high_water > self.request_channel_capacity {
            return Err(bad(
                "send_high_water",
                "must not exceed request_channel_capacity (the watermark must degrade \
                 before the channel can block the producer)",
            ));
        }
        if self.inflight == 0 {
            return Err(bad("inflight", "must be at least 1"));
        }
        if let Some(tls) = &self.tls {
            tls.validate()
                .map_err(|e| DaemonError::ConfigError(format!("outlet `{}`: {}", self.name, e)))?;
        }
        Ok(())
    }

    /// 构造 rumqttc [`MqttOptions`]（含 TLS 传输与凭证）。
    ///
    /// # Errors
    /// 配置非法 → [`DaemonError::ConfigError`]；证书读取失败 → [`DaemonError::SecurityError`]。
    pub fn build_options(&self) -> DaemonResult<MqttOptions> {
        self.validate()?;

        // task 61 适配：rumqttc-next 把「broker + port」合并成 `Broker`（`From<(S, u16)>`），
        // `MqttOptions::new` 由 3 参降为 2 参；`set_keep_alive` 由 `Duration` 改为 **u16 秒**；
        // `set_credentials` 的口令类型由 `Vec<u8>` 改为 `bytes::Bytes`。
        // 语义对齐：`(host, port)` 即旧 `new(client_id, broker, port)` 的等价展开；
        // 口令 `as_bytes()` 与旧 `String → Vec<u8>` 的字节内容一致。
        let mut options = MqttOptions::new(&self.client_id, (self.broker.as_str(), self.port));
        // MQTT 的 keep alive 字段本身是 u16 秒，旧版 rumqttc 也是写包时才截断到秒，
        // 故此处先截秒与旧行为等价；超出 u16 直接前置报错，避免静默回绕。
        options.set_keep_alive(u16::try_from(self.keep_alive.as_secs()).map_err(|_| {
            DaemonError::ConfigError(format!(
                "keep_alive {}s exceeds the u16 second limit (65535)",
                self.keep_alive.as_secs()
            ))
        })?);
        options.set_clean_session(self.clean_session);
        options.set_request_channel_capacity(self.request_channel_capacity);
        options.set_inflight(self.inflight);

        if let (Some(username), Some(password)) = (&self.username, &self.password) {
            // `Bytes` 要求 'static，故此处取自有副本（与旧版 `String → Vec<u8>` 一样是拥有语义）。
            options.set_credentials(username, password.as_bytes().to_vec());
        }
        if let Some(tls) = &self.tls {
            options.set_transport(tls.to_transport()?);
        }
        Ok(options)
    }
}

// ---- 载荷编码器契约（实现 = task 62） ----

/// 北向载荷编码器（**实现归 task 62**，本模块只定义契约并消费）。
///
/// `publish_sample` 要求编码器声明的 [`Encoding`] 与本路连接一致，
/// 不一致即拒绝发布（防止 protobuf 字节被推到 JSON 出口）。
pub trait PayloadEncoder: Send + Sync {
    /// 该编码器产出的编码格式。
    fn encoding(&self) -> Encoding;

    /// 把一个 [`ProcessedSample`] 序列化为该格式的字节。
    ///
    /// # Errors
    /// 序列化失败返回对应错误域（编码侧自行决定）。
    fn encode(&self, sample: &ProcessedSample) -> DaemonResult<Vec<u8>>;
}

// ---- 慢消费者保护（task 54：发送通道水位 + 落盘降级回调） ----

/// 慢消费者降级回调（task 54）：发送积压超水位时被调用。
///
/// 实现方应在该回调里把报文转入**离线落盘降级**路径（如 [`crate::offline_queue::
/// OfflineQueue`]），绝不阻塞——回调约定为同步、快速、无网络 IO。
pub trait SlowConsumerSink: Send + Sync {
    /// 未确认积压达到水位阈值时触发。
    ///
    /// `outstanding`：触发时的未确认积压数（已提交未收到 PUBACK/PUBCOMP 的报文数）。
    fn on_slow_consumer(&self, outstanding: usize);
}

/// 带水位保护的发布结果（task 54）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishOutcome {
    /// 已提交到发送通道（等待事件循环投递与确认）。
    Sent,
    /// 积压超水位 → 已回调降级，本条**未发送**（由调用方落盘兜底）。
    Degraded {
        /// 触发时的未确认积压数。
        outstanding: usize,
    },
}

// ==================== 背压接线（task 54） ====================
//
// 把 `crate::backpressure` 里「可测但没接线」的原语真正接进北向出站路径。
// 三个接线点（与 brief 一一对应）：
//   1. 发送队列水位 + 慢消费者保护（落盘降级）→ `NorthSendQueue` + `MqttClient::pump_send`
//      + `MqttClient::poll_event` 的 PUBACK→`confirm`
//   2. 补发路径幂等（先落 Ack 后推位点）→ `NorthReplay` / `IngestGate` + `MqttClient::pump_replay`
//   3. 审计取走上报出口 → `AuditPump` + `MqttClient::drain_audit`

/// 审计上报出口（task 54 接线点 3）。
///
/// 接收 [`AuditLog::drain`] 取走的审计事件（落盘降级 / 溢出拒绝 / 水位进出）。
/// 约定实现**同步、快速、无阻塞**：审计环本身有界，慢消费由 [`AuditPump::dropped`] 可观测。
pub trait AuditSink: Send + Sync {
    /// 上报一批审计事件（`events` 保证非空）。
    fn emit(&self, events: Vec<BackpressureAudit>);
}

/// 取锁并在中毒时取回内部数据（**绝不 panic**）。
fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => PoisonError::into_inner(poisoned),
    }
}

/// 审计取走上报泵：`AuditLog::drain()` 的调用点。
///
/// 审计环有界（默认 4096）：若长期无人 [`Self::pump`]，事件会被静默淘汰——
/// [`Self::dropped`] > 0 即「审计消费变慢」的可观测信号。
pub struct AuditPump {
    log: Arc<AuditLog>,
    sink: Arc<dyn AuditSink>,
}

impl AuditPump {
    /// 构造。
    #[must_use]
    pub fn new(log: Arc<AuditLog>, sink: Arc<dyn AuditSink>) -> Self {
        Self { log, sink }
    }

    /// 底层审计环（供快照 / 运维观测）。
    #[must_use]
    pub fn log(&self) -> &Arc<AuditLog> {
        &self.log
    }

    /// 取走全部待上报审计并交给 [`AuditSink`]，返回本次上报条数。
    pub fn pump(&self) -> usize {
        let events = self.log.drain();
        let count = events.len();
        if count > 0 {
            self.sink.emit(events);
        }
        count
    }

    /// 累计产生条数。
    #[must_use]
    pub fn emitted(&self) -> u64 {
        self.log.emitted()
    }

    /// 因审计环满被淘汰的条数（> 0 说明审计消费变慢）。
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.log.dropped()
    }
}

impl fmt::Debug for AuditPump {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuditPump")
            .field("buffered", &self.log.len())
            .field("emitted", &self.emitted())
            .field("dropped", &self.dropped())
            .finish_non_exhaustive()
    }
}

/// 北向发送队列（task 54 接线点 1）：有界内存 + 超限落盘降级 + PUBACK 回收。
///
/// **永不阻塞采集路径**：`push` 只做一次水位比较 + 一次有界落盘调用。
/// 内部以 [`Mutex`] 包住 [`SendQueue`]，使发布侧（`&self`）也能安全推进。
///
/// 水位取 [`SendQueue::with_defaults`]（高水位 32 / 硬上限 64，设计文档 §4）；
/// 落盘降级走 [`QueueSpillSink`] → [`OfflineQueue::enqueue`]（分配新 `batch_seq`）。
///
/// ⚠ **入参契约**：`push` 的负载必须是**尚未持久化**的新数据（采集 / 管道出口）。
/// 不要把 [`OfflineQueue`] 中已存在的批次再 push 进来——落盘降级会再写一行，
/// 造成重复数据；补发路径请用 [`NorthReplay`]（失败只回灌内存，不落盘）。
pub struct NorthSendQueue {
    inner: Mutex<SendQueue>,
    audit: Arc<AuditLog>,
    clock: Arc<dyn Clock>,
}

impl NorthSendQueue {
    /// 构造（落盘降级与审计环绑定到同一 [`OfflineQueue`] / [`AuditLog`]）。
    // TODO(接线后续): 配置段接入点 —— 水位当前固定取 with_defaults 的 32/64，
    // 待 config.rs 提供 `[backpressure] send_high_water / send_hard_limit` 后改为注入。
    #[must_use]
    pub fn new(queue: Arc<OfflineQueue>, audit: Arc<AuditLog>, clock: Arc<dyn Clock>) -> Self {
        let spill = Arc::new(QueueSpillSink::new(queue));
        let inner = SendQueue::with_defaults(spill, Arc::clone(&audit), Arc::clone(&clock));
        Self {
            inner: Mutex::new(inner),
            audit,
            clock,
        }
    }

    /// 当前时钟读数（构造 [`PendingSend::new`] 的 `enqueued_ns`）。
    #[must_use]
    pub fn now_ns(&self) -> i64 {
        self.clock.now_ns()
    }

    /// 提交一条待发送负载（**同步、永不阻塞**）。
    pub fn push(&self, seq: u64, payload: Vec<u8>) -> PushOutcome {
        let item = PendingSend::new(seq, payload, self.clock.now_ns());
        self.push_item(item)
    }

    /// 提交一条已构造的待发送条目。
    pub fn push_item(&self, item: PendingSend) -> PushOutcome {
        lock_or_recover(&self.inner).push(item)
    }

    /// 取出最多 `max` 条交给网络层（`ready` → `sent`，**水位占用不变**）。
    pub fn take_ready(&self, max: usize) -> Vec<PendingSend> {
        lock_or_recover(&self.inner).take_ready(max)
    }

    /// PUBACK / PUBCOMP 到达：从在途窗口移除 `count` 条（按提交顺序），返回实际移除条数。
    ///
    /// **只有确认到达才调用**——未确认的条目继续占用水位，因此不会被重复发布。
    pub fn confirm(&self, count: usize) -> usize {
        lock_or_recover(&self.inner).confirm(count)
    }

    /// 发布失败回灌（`sent` → `ready` 头部），水位占用不变。
    pub fn requeue_failed(&self, items: Vec<PendingSend>) {
        lock_or_recover(&self.inner).requeue_failed(items);
    }

    /// 在途（已提交未确认）条数（水位口径 = `ready` + `sent`）。
    #[must_use]
    pub fn pending(&self) -> usize {
        lock_or_recover(&self.inner).pending()
    }

    /// 在途字节数。
    #[must_use]
    pub fn bytes(&self) -> usize {
        lock_or_recover(&self.inner).bytes()
    }

    /// 当前水位等级。
    #[must_use]
    pub fn level(&self) -> WaterLevel {
        lock_or_recover(&self.inner).level()
    }

    /// 统计快照（入内存 / 落盘降级 / 拒绝 / 在途）。
    #[must_use]
    pub fn stats(&self) -> SendQueueStats {
        lock_or_recover(&self.inner).stats()
    }

    /// 共享审计环。
    #[must_use]
    pub fn audit(&self) -> &Arc<AuditLog> {
        &self.audit
    }
}

impl fmt::Debug for NorthSendQueue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NorthSendQueue")
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

/// 补发控制器（task 54 接线点 2）：批次级幂等 + 「先落 Ack 后推位点」。
///
/// Ack 持久化走 [`QueueAckSink`] → [`OfflineQueue::ack_up_to`]（其内部顺序同样是
/// **先落地确认记录，成功后才推进内存位点**）；[`ReplayController::ack`] 不得颠倒该顺序。
pub struct NorthReplay {
    controller: Mutex<ReplayController>,
    queue: Arc<OfflineQueue>,
    audit: Arc<AuditLog>,
}

impl NorthReplay {
    /// 构造（起始位点取 `queue.ack_seq()`，即重启后读回的 high-water mark）。
    #[must_use]
    pub fn new(
        gateway_id: impl Into<String>,
        queue: Arc<OfflineQueue>,
        audit: Arc<AuditLog>,
    ) -> Self {
        let start = queue.ack_seq();
        let sink = Arc::new(QueueAckSink::new(Arc::clone(&queue)));
        Self {
            controller: Mutex::new(ReplayController::new(gateway_id, sink, start)),
            queue,
            audit,
        }
    }

    /// 当前 high-water mark（已确认到的最大 `batch_seq`）。
    #[must_use]
    pub fn high_water_mark(&self) -> u64 {
        lock_or_recover(&self.controller).high_water_mark()
    }

    /// 该 `seq` 是否需要补发（`seq > high_water` 且去重窗口内未发过）。
    #[must_use]
    pub fn should_send(&self, seq: u64) -> bool {
        lock_or_recover(&self.controller).should_send(seq)
    }

    /// 标记已发送（去重判定）：返回 [`DedupOutcome::Applied`] 才真正发送。
    pub fn mark_sent(&self, seq: u64) -> DedupOutcome {
        lock_or_recover(&self.controller).mark_sent(seq)
    }

    /// 按幂等键 `{gateway_id}:{batch_seq}` 标记已发送。
    pub fn mark_sent_key(&self, key: &str) -> DedupOutcome {
        lock_or_recover(&self.controller).mark_sent_key(key)
    }

    /// 确认：**先落 Ack，成功后才推进位点**（顺序固定，不得颠倒）。
    ///
    /// # Errors
    /// Ack 持久化失败 → 位点与去重窗口均不动，数据继续保留，返回底层 `StorageError`（4000）。
    pub fn ack(&self, seq: u64) -> DaemonResult<AckOutcome> {
        lock_or_recover(&self.controller).ack(seq)
    }

    /// 取一批待补发条目：`seq > high_water` 且去重窗口内未发过（按 `seq` 升序）。
    ///
    /// # Errors
    /// 队列写线程不可用 → `StorageError`（4000）。
    pub fn next_replay_batch(&self, max: usize) -> DaemonResult<Vec<QueuedBatch>> {
        let batches = self.queue.replay_batch(max)?;
        Ok(batches
            .into_iter()
            .filter(|b| self.should_send(b.seq))
            .collect())
    }

    /// 去重登记簿统计（`applied_rows` / `duplicate_rows` / `high_water`）。
    #[must_use]
    pub fn stats(&self) -> LedgerStats {
        lock_or_recover(&self.controller).ledger().stats()
    }

    /// 底层离线队列。
    #[must_use]
    pub fn queue(&self) -> &Arc<OfflineQueue> {
        &self.queue
    }

    /// 共享审计环。
    #[must_use]
    pub fn audit(&self) -> &Arc<AuditLog> {
        &self.audit
    }
}

impl fmt::Debug for NorthReplay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NorthReplay")
            .field("high_water", &self.high_water_mark())
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

/// 消费端 / 出口侧幂等闸门（task 54 接线点 2 的消费侧参考接线）。
///
/// 入库前调用 [`Self::admit`]：**只有 [`DedupOutcome::Applied`] 才入库**，
/// [`DedupOutcome::Duplicate`] 直接跳过（忽略重复 = 计划允许的唯一隐式丢弃）。
pub struct IngestGate {
    ledger: Mutex<IdempotencyLedger>,
    audit: Arc<AuditLog>,
}

impl IngestGate {
    /// 构造（去重窗口取 [`DEFAULT_MEM_HIGH_WATER_ROWS`]）。
    #[must_use]
    pub fn new(gateway_id: impl Into<String>, audit: Arc<AuditLog>) -> Self {
        let ledger = IdempotencyLedger::new(gateway_id, DEFAULT_MEM_HIGH_WATER_ROWS);
        Self {
            ledger: Mutex::new(ledger),
            audit,
        }
    }

    /// 按幂等键 `{gateway_id}:{batch_seq}` 判定是否入库。
    pub fn admit(&self, key: &str) -> DedupOutcome {
        lock_or_recover(&self.ledger).apply_key(key)
    }

    /// 按 `batch_seq` 判定是否入库。
    pub fn admit_seq(&self, seq: u64) -> DedupOutcome {
        lock_or_recover(&self.ledger).apply_seq(seq)
    }

    /// 按批次判定是否入库（复用 [`QueuedBatch::idempotency_key`]）。
    pub fn admit_batch(&self, batch: &QueuedBatch) -> DedupOutcome {
        lock_or_recover(&self.ledger).apply_batch(batch)
    }

    /// 入库成功后推进位点并裁剪去重窗口（**只能在入库成功后调用**）。
    pub fn confirm_up_to(&self, seq: u64) {
        lock_or_recover(&self.ledger).confirm_up_to(seq);
    }

    /// 当前 high-water mark。
    #[must_use]
    pub fn high_water_mark(&self) -> u64 {
        lock_or_recover(&self.ledger).high_water()
    }

    /// 去重登记簿统计。
    #[must_use]
    pub fn stats(&self) -> LedgerStats {
        lock_or_recover(&self.ledger).stats()
    }

    /// 共享审计环。
    #[must_use]
    pub fn audit(&self) -> &Arc<AuditLog> {
        &self.audit
    }
}

impl fmt::Debug for IngestGate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IngestGate")
            .field("high_water", &self.high_water_mark())
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

/// 北向出口背压接线束（task 54）：把三个接线点组装到一处并挂到 [`MqttClient`]。
///
/// 三者共享同一 [`OfflineQueue`] 与同一 [`AuditLog`]，形成闭环：
/// 发送队列超限 → 落盘降级进队列 → 补发路径按幂等键重放 → 审计统一 [`AuditPump::pump`] 上报。
pub struct NorthOutlet {
    send: Arc<NorthSendQueue>,
    replay: Arc<NorthReplay>,
    ingest: Arc<IngestGate>,
    audit: Arc<AuditPump>,
}

impl NorthOutlet {
    /// 构造（审计环容量取 [`AuditLog`] 默认 4096）。
    #[must_use]
    pub fn new(
        gateway_id: impl Into<String>,
        queue: Arc<OfflineQueue>,
        clock: Arc<dyn Clock>,
        audit_sink: Arc<dyn AuditSink>,
    ) -> Self {
        Self::with_audit_log(
            gateway_id,
            queue,
            clock,
            Arc::new(AuditLog::default()),
            audit_sink,
        )
    }

    /// 构造并注入审计环（测试可用小容量断言「审计消费变慢」= `dropped() > 0`）。
    #[must_use]
    pub fn with_audit_log(
        gateway_id: impl Into<String>,
        queue: Arc<OfflineQueue>,
        clock: Arc<dyn Clock>,
        audit_log: Arc<AuditLog>,
        audit_sink: Arc<dyn AuditSink>,
    ) -> Self {
        let gateway_id = gateway_id.into();
        let pump_log = Arc::clone(&audit_log);
        Self {
            send: Arc::new(NorthSendQueue::new(
                Arc::clone(&queue),
                Arc::clone(&audit_log),
                clock,
            )),
            replay: Arc::new(NorthReplay::new(
                gateway_id.clone(),
                queue,
                Arc::clone(&audit_log),
            )),
            ingest: Arc::new(IngestGate::new(gateway_id, audit_log)),
            audit: Arc::new(AuditPump::new(pump_log, audit_sink)),
        }
    }

    /// 接线点 1：有界发送队列（水位 + 落盘降级）。
    #[must_use]
    pub fn send(&self) -> &Arc<NorthSendQueue> {
        &self.send
    }

    /// 接线点 2：补发 / 幂等控制器。
    #[must_use]
    pub fn replay(&self) -> &Arc<NorthReplay> {
        &self.replay
    }

    /// 接线点 2（消费侧）：入库前幂等闸门。
    #[must_use]
    pub fn ingest(&self) -> &Arc<IngestGate> {
        &self.ingest
    }

    /// 接线点 3：审计取走上报泵。
    #[must_use]
    pub fn audit(&self) -> &Arc<AuditPump> {
        &self.audit
    }

    /// 共享审计环。
    #[must_use]
    pub fn audit_log(&self) -> &Arc<AuditLog> {
        self.audit.log()
    }
}

impl fmt::Debug for NorthOutlet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NorthOutlet")
            .field("send", &self.send)
            .field("replay", &self.replay)
            .field("audit", &self.audit)
            .finish_non_exhaustive()
    }
}

/// 发送泵一轮的结果（task 54 接线点 1）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SendPumpReport {
    /// 本轮从 `ready` 取出的条数。
    pub taken: usize,
    /// 成功提交到发送通道的条数（等待 PUBACK）。
    pub published: usize,
    /// 发布失败被回灌到 `ready` 头部的条数（含未处理的剩余条目）。
    pub requeued: usize,
}

/// 补发泵一轮的结果（task 54 接线点 2）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReplayPumpReport {
    /// 通过 `should_send` 的补发候选条数。
    pub candidates: usize,
    /// 实际发布成功条数。
    pub sent: usize,
    /// 被幂等账本判定为重复而跳过的条数。
    pub deduped: usize,
    /// 发布失败条数（该批仍留在离线队列中，位点未推进）。
    pub failed: usize,
    /// 是否推进了位点（Ack 已落盘）。
    pub acked: bool,
    /// Ack 落盘失败次数（> 0 = 位点未推进，重放将由幂等键去重）。
    pub ack_errors: usize,
}

/// 一轮完整泵（发送 + 补发 + 审计上报）的结果。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PumpReport {
    /// 发送队列一轮。
    pub send: SendPumpReport,
    /// 补发一轮。
    pub replay: ReplayPumpReport,
    /// 本轮取走并上报的审计条数。
    pub audit_emitted: usize,
}

// ---- 单路客户端 ----

/// 单路 MQTT 客户端（持有 rumqttc [`AsyncClient`] + [`EventLoop`]）。
///
/// 生命周期：`new` 只构建（不发起连接）→ 首次 `poll_event()` 触发连接并产出
/// `ConnAck` → 出错时记录退避 → 继续 `poll_event()` 即自动重连。
pub struct MqttClient {
    /// 该路连接的配置（含 encoding 声明）。
    endpoint: EndpointConfig,
    /// 发布/订阅句柄（内部走 flume 通道，不阻塞事件循环）。
    client: AsyncClient,
    /// 事件循环（手动推进；`pending` 承载会话恢复后待重发的报文）。
    eventloop: EventLoop,
    /// 重连退避（复用南向 `Reconnector`，纯逻辑）。
    reconnector: Reconnector,
    /// 最近一次 CONNACK 的 `session_present`（会话恢复观测点）。
    session_present: bool,
    /// 是否已建立 MQTT 会话（收到 CONNACK 且未被错误打断）。
    connected: bool,
    /// 累计 CONNACK 次数（首次为 1；`reconnect_count()` = 该值 - 1）。
    connect_count: u64,
    /// 待重连退避间隔（`Some` 表示上次 poll 失败，调用方应先 `backoff()`）。
    retry_after: Option<Duration>,
    /// 累计已提交的报文数（慢消费者水位记账；task 54）。
    submitted: u64,
    /// 累计已确认的报文数（PUBACK / PUBCOMP；task 54）。
    acked: u64,
    /// 已提交未确认报文的来源标记（task 54）：`true` = 出自发送队列，
    /// PUBACK 到达时需 `confirm` 回收其水位占用；`false` = 补发 / 水位降级路径直发。
    ///
    /// 与 PUBACK **严格 FIFO** 对应（QoS 0 无确认语义，故不入队）。
    ack_sources: VecDeque<bool>,
    /// 北向背压接线束（task 54；`None` = 未接线，全部泵为空操作）。
    outlet: Option<Arc<NorthOutlet>>,
}

impl MqttClient {
    /// 构建客户端（**不发起连接**，连接由首次 [`Self::poll_event`] 触发）。
    ///
    /// # Errors
    /// 配置非法 → [`DaemonError::ConfigError`]；证书问题 → [`DaemonError::SecurityError`]。
    pub fn new(endpoint: EndpointConfig) -> DaemonResult<Self> {
        let options = endpoint.build_options()?;
        let capacity = endpoint.request_channel_capacity;
        let reconnector = Reconnector::new(
            endpoint.reconnect_initial,
            endpoint.reconnect_max,
            endpoint.reconnect_multiplier,
        );
        let (client, eventloop) = AsyncClient::new(options, capacity);
        Ok(Self {
            endpoint,
            client,
            eventloop,
            reconnector,
            session_present: false,
            connected: false,
            connect_count: 0,
            retry_after: None,
            submitted: 0,
            acked: 0,
            ack_sources: VecDeque::new(),
            outlet: None,
        })
    }

    /// 该路连接的配置（含 `encoding`）。
    pub fn endpoint(&self) -> &EndpointConfig {
        &self.endpoint
    }

    /// 该路连接声明的载荷编码。
    pub fn encoding(&self) -> Encoding {
        self.endpoint.encoding
    }

    /// 该路连接的默认 QoS。
    pub fn qos(&self) -> QoS {
        self.endpoint.qos
    }

    /// 最近一次 CONNACK 的 `session_present`（`true` = 会话已恢复）。
    pub fn session_present(&self) -> bool {
        self.session_present
    }

    /// 是否已建立会话。
    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// 累计重连次数（CONNACK 次数 - 1）。
    pub fn reconnect_count(&self) -> u64 {
        self.connect_count.saturating_sub(1)
    }

    /// 待重连退避间隔（上次 poll 失败后置位，成功后清除）。
    pub fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }

    /// 会话恢复后待重发的报文数（`EventLoop` 待重传队列长度）；
    /// task 61 适配：`pending` 字段已随 rumqttc-next 变私有，改用公开访问器 `pending_len()`。
    pub fn pending_requests(&self) -> usize {
        self.eventloop.pending_len()
    }

    /// 未确认积压（task 54）：已提交但尚未收到 PUBACK / PUBCOMP 的报文数。
    ///
    /// 慢消费者水位的观测量：`submit` 时 +1，`poll_event` 收到确认时 -1。
    /// QoS 0 报文不记账（无确认语义，broker 即时消费）。
    pub fn outstanding(&self) -> usize {
        self.submitted.saturating_sub(self.acked) as usize
    }

    // ---- 背压接线（task 54） ----

    /// 挂载北向背压接线束（task 54）。未挂载时全部泵为无操作。
    #[must_use]
    pub fn with_outlet(mut self, outlet: Arc<NorthOutlet>) -> Self {
        self.outlet = Some(outlet);
        self
    }

    /// 已挂载的背压接线束（未挂载为 `None`）。
    #[must_use]
    pub fn outlet(&self) -> Option<&Arc<NorthOutlet>> {
        self.outlet.as_ref()
    }

    /// 发送队列在途（已提交未确认）条数（task 54 水位口径）。
    #[must_use]
    pub fn queued_pending(&self) -> usize {
        self.outlet
            .as_ref()
            .map_or(0, |outlet| outlet.send().pending())
    }

    /// 已提交、正等待 PUBACK / PUBCOMP 的报文的确认来源标记数（可观测）。
    #[must_use]
    pub fn ack_tracked(&self) -> usize {
        self.ack_sources.len()
    }

    /// 记账一条已提交报文（task 54）。
    ///
    /// `from_send_queue` 决定 PUBACK 到达时是否回收 [`NorthSendQueue`] 的在途窗口：
    /// 只有出自发送队列的报文才占用其水位，补发路径直发的报文不得占用。
    /// QoS 0 无确认语义 → 不入确认来源队列（避免其无界增长）。
    fn note_submitted(&mut self, from_send_queue: bool) {
        self.submitted = self.submitted.saturating_add(1);
        if self.endpoint.qos != QoS::AtMostOnce {
            self.ack_sources.push_back(from_send_queue);
        }
    }

    /// 提交一条负载到有界发送队列（task 54 接线点 1 入口；**同步、永不阻塞**）。
    ///
    /// # Errors
    /// 未挂载 [`NorthOutlet`] → `ConfigError`（2000）。
    ///
    /// 硬上限且落盘也失败**不是错误**：返回 [`PushOutcome::Rejected`] 并交还数据
    /// （已记审计），由调用方决定计数 / 告警 / 重试——**绝不静默丢**。
    pub fn submit(&self, seq: u64, payload: Vec<u8>) -> DaemonResult<PushOutcome> {
        let outlet = self.outlet.as_ref().ok_or_else(|| {
            DaemonError::ConfigError(
                "north outlet backpressure not attached (use `with_outlet`)".to_string(),
            )
        })?;
        Ok(outlet.send().push(seq, payload))
    }

    /// 取走并上报全部背压审计（task 54 接线点 3：`AuditLog::drain()` 的调用点）。
    #[must_use]
    pub fn drain_audit(&self) -> usize {
        self.outlet
            .as_ref()
            .map_or(0, |outlet| outlet.audit().pump())
    }

    /// 推进发送队列一轮：`take_ready` → 发布 → 失败 `requeue_failed`（task 54 接线点 1）。
    ///
    /// 水位口径 = 已提交未确认（PUBACK / PUBCOMP）条数；在途达水位时**不发布**，
    /// 返回空报告（慢消费者保护，绝不阻塞）。
    ///
    /// 发布失败属可重试暂态，**不升级为错误**：失败条目与未处理的剩余条目一并
    /// 回灌 `ready` 头部（水位占用不变，不丢数据），调用方据
    /// [`SendPumpReport::requeued`] > 0 决定退避。未挂载 [`NorthOutlet`] → 空报告。
    pub async fn pump_send(&mut self, topic_prefix: &str) -> SendPumpReport {
        let Some(outlet) = self.outlet.clone() else {
            return SendPumpReport::default();
        };
        let headroom = self
            .endpoint
            .send_high_water
            .saturating_sub(self.outstanding());
        if headroom == 0 {
            return SendPumpReport::default();
        }
        let qos = self.endpoint.qos;
        let mut queue: VecDeque<PendingSend> = outlet.send().take_ready(headroom).into();
        let taken = queue.len();
        let mut published = 0usize;
        let mut requeued = 0usize;
        while let Some(item) = queue.pop_front() {
            let topic = format!("{topic_prefix}/{}", item.seq);
            let payload = item.payload.clone();
            match self.publish_mut(&topic, payload, qos).await {
                Ok(()) => {
                    self.note_submitted(true);
                    published = published.saturating_add(1);
                }
                Err(err) => {
                    tracing::warn!(
                        batch_seq = item.seq,
                        error = %err,
                        "north send failed; requeueing unacked batches (no data dropped)"
                    );
                    let mut failed: Vec<PendingSend> = Vec::with_capacity(queue.len() + 1);
                    failed.push(item);
                    failed.extend(queue.drain(..));
                    requeued = failed.len();
                    outlet.send().requeue_failed(failed);
                    break;
                }
            }
        }
        SendPumpReport {
            taken,
            published,
            requeued,
        }
    }

    /// 推进补发一轮：`should_send` → 去重判定 → 发布 → **发布成功后 `ack`**
    /// （task 54 接线点 2）。未挂载 [`NorthOutlet`] → 空报告。
    ///
    /// 顺序红线：`ack` 内部为「先落 Ack，成功后才推位点」，**不得颠倒**。
    /// Ack 落盘失败 → 位点不动（`ack_errors` +1），该批仍可重放并由幂等键去重。
    /// 发布失败 → 该批留在离线队列中不回灌、不落盘（避免重复行）。
    ///
    /// # Errors
    /// 离线队列写线程不可用 → `StorageError`（4000）。
    pub async fn pump_replay(&mut self, topic_prefix: &str) -> DaemonResult<ReplayPumpReport> {
        let Some(outlet) = self.outlet.clone() else {
            return Ok(ReplayPumpReport::default());
        };
        let headroom = self
            .endpoint
            .send_high_water
            .saturating_sub(self.outstanding());
        if headroom == 0 {
            return Ok(ReplayPumpReport::default());
        }
        let batches = outlet.replay().next_replay_batch(headroom)?;
        let qos = self.endpoint.qos;
        let mut report = ReplayPumpReport {
            candidates: batches.len(),
            ..ReplayPumpReport::default()
        };
        let mut last_acked: u64 = 0;
        for batch in batches {
            // 幂等去重：只有 `Applied` 才发布（`Duplicate` 跳过，绝不重发）。
            if outlet.replay().mark_sent(batch.seq).is_duplicate() {
                report.deduped = report.deduped.saturating_add(1);
                continue;
            }
            let topic = format!("{topic_prefix}/{}", batch.seq);
            match self.publish_mut(&topic, batch.payload.clone(), qos).await {
                Ok(()) => {
                    self.note_submitted(false);
                    report.sent = report.sent.saturating_add(1);
                    last_acked = last_acked.max(batch.seq);
                }
                Err(err) => {
                    tracing::warn!(
                        batch_seq = batch.seq,
                        error = %err,
                        "north replay publish failed; batch stays unacked in the offline queue"
                    );
                    report.failed = report.failed.saturating_add(1);
                    // 只回灌内存发送队列，**不落盘**（该批仍在离线队列里，落盘会产生重复行）。
                    outlet.send().requeue_failed(vec![PendingSend::new(
                        batch.seq,
                        batch.payload,
                        outlet.send().now_ns(),
                    )]);
                    break;
                }
            }
        }
        if last_acked > 0 {
            // 顺序红线：ReplayController::ack 内部「先落 Ack，后推位点」，不得颠倒。
            match outlet.replay().ack(last_acked) {
                Ok(AckOutcome::Advanced { .. }) => report.acked = true,
                Ok(AckOutcome::AlreadyAcked { .. }) => {}
                Err(err) => {
                    report.ack_errors = report.ack_errors.saturating_add(1);
                    tracing::warn!(
                        batch_seq = last_acked,
                        error = %err,
                        "north replay ack not persisted; cursor unchanged \
                         (replay will be deduped by idempotency key)"
                    );
                }
            }
        }
        Ok(report)
    }

    /// 一轮完整泵：发送 → 补发 → 审计上报（task 54 三个接线点各推进一次）。
    ///
    /// # Errors
    /// 离线队列不可用（[`Self::pump_replay`]）→ `StorageError`（4000）。
    pub async fn pump(&mut self, topic_prefix: &str) -> DaemonResult<PumpReport> {
        let send = self.pump_send(topic_prefix).await;
        let replay = self.pump_replay(topic_prefix).await?;
        let audit_emitted = self.drain_audit();
        Ok(PumpReport {
            send,
            replay,
            audit_emitted,
        })
    }

    /// 带慢消费者水位保护的发布（task 54）。
    ///
    /// 水位判定只是一次整数比较（O(1)，无 IO、无 await 前置阻塞）：
    /// - 未确认积压 `outstanding >= send_high_water` → **不提交**到发送通道，
    ///   调用注入的 [`SlowConsumerSink`]（调用方在此走离线落盘降级），返回
    ///   [`PublishOutcome::Degraded`]；
    /// - 否则正常发布并返回 [`PublishOutcome::Sent`]。
    ///
    /// 水位 < 请求通道容量（构建期校验），保证降级先于通道阻塞生效——
    /// 发送侧永不阻塞超过水位判断所需时间。
    ///
    /// # Errors
    /// 发布失败（通道关闭）→ [`DaemonError::MqttError`]（此时不计入积压）。
    pub async fn publish_backpressured(
        &mut self,
        topic: &str,
        payload: impl Into<Vec<u8>>,
        degrade: &dyn SlowConsumerSink,
    ) -> DaemonResult<PublishOutcome> {
        let outstanding = self.outstanding();
        if outstanding >= self.endpoint.send_high_water {
            degrade.on_slow_consumer(outstanding);
            return Ok(PublishOutcome::Degraded { outstanding });
        }
        let qos = self.endpoint.qos;
        self.publish_mut(topic, payload.into(), qos).await?;
        self.note_submitted(false);
        Ok(PublishOutcome::Sent)
    }

    /// 按该路连接的默认 QoS 发布一条原始报文。
    ///
    /// # Errors
    /// 请求通道关闭 → [`DaemonError::MqttError`]。
    pub async fn publish(&self, topic: &str, payload: impl Into<Vec<u8>>) -> DaemonResult<()> {
        self.publish_with_qos(topic, payload, self.endpoint.qos)
            .await
    }

    /// 按指定 QoS 发布一条原始报文（覆盖该路默认 QoS）。
    ///
    /// # Errors
    /// 请求通道关闭 → [`DaemonError::MqttError`]。
    pub async fn publish_with_qos(
        &self,
        topic: &str,
        payload: impl Into<Vec<u8>>,
        qos: QoS,
    ) -> DaemonResult<()> {
        self.client
            .publish(topic, qos, false, payload.into())
            .await
            .map_err(|e| DaemonError::MqttError(format!("publish `{topic}` failed: {e}")))
    }

    /// 以 `&mut self` 发布一条原始报文（**驱动任务唯一发布入口**）。
    ///
    /// 语义与 [`Self::publish_with_qos`] 完全一致；区别仅在借用形式：驱动任务的
    /// future 必须是 `Send`，而 [`MqttClient`] 因持有 rumqttc `EventLoop`
    /// （内含 `!Sync` 的传输对象）而为 `!Sync` —— `&self` 跨 `.await` 会让
    /// future 非 `Send`（无法 `tokio::spawn`）；持 `&mut self` 则只要求
    /// `MqttClient: Send`（成立），故驱动路径统一走该方法。
    ///
    /// # Errors
    /// 请求通道关闭 → [`DaemonError::MqttError`]。
    async fn publish_mut(&mut self, topic: &str, payload: Vec<u8>, qos: QoS) -> DaemonResult<()> {
        self.client
            .publish(topic, qos, false, payload)
            .await
            .map_err(|e| DaemonError::MqttError(format!("publish `{topic}` failed: {e}")))
    }

    /// 发布一个 [`ProcessedSample`]：主题按 `topic_for` 拼接，载荷由注入的编码器产出。
    ///
    /// **编码一致性校验**：`encoder.encoding()` 必须等于本路连接的 `encoding`，
    /// 否则返回 [`DaemonError::ConfigError`]（错误码 2000）。
    ///
    /// # Errors
    /// 编码不匹配 → `ConfigError`；序列化失败 → 编码器错误；通道关闭 → `MqttError`。
    pub async fn publish_sample(
        &self,
        sample: &ProcessedSample,
        encoder: &dyn PayloadEncoder,
    ) -> DaemonResult<()> {
        if encoder.encoding() != self.endpoint.encoding {
            return Err(DaemonError::ConfigError(format!(
                "outlet `{}`: encoder declares `{}` but connection declares `{}`",
                self.endpoint.name,
                encoder.encoding(),
                self.endpoint.encoding
            )));
        }
        let topic = self.endpoint.topic_for(&sample.device_id, &sample.point_id);
        let payload = encoder.encode(sample)?;
        self.publish(&topic, payload).await
    }

    /// 订阅主题（`qos` 为该订阅的期望等级）。
    ///
    /// # Errors
    /// 请求通道关闭 → [`DaemonError::MqttError`]。
    pub async fn subscribe(&self, topic: &str, qos: QoS) -> DaemonResult<()> {
        self.client
            .subscribe(topic, qos)
            .await
            .map_err(|e| DaemonError::MqttError(format!("subscribe `{topic}` failed: {e}")))
    }

    /// 取消订阅。
    ///
    /// # Errors
    /// 请求通道关闭 → [`DaemonError::MqttError`]。
    pub async fn unsubscribe(&self, topic: &str) -> DaemonResult<()> {
        self.client
            .unsubscribe(topic)
            .await
            .map_err(|e| DaemonError::MqttError(format!("unsubscribe `{topic}` failed: {e}")))
    }

    /// 主动断开（发送 DISCONNECT，正常下线语义，不触发重连）。
    ///
    /// # Errors
    /// 请求通道关闭 → [`DaemonError::MqttError`]。
    pub async fn disconnect(&self) -> DaemonResult<()> {
        self.client
            .disconnect()
            .await
            .map_err(|e| DaemonError::MqttError(format!("disconnect failed: {e}")))
    }

    /// 推进一轮事件循环。
    ///
    /// - 首次调用触发连接并产出 `Incoming(ConnAck)`；
    /// - 成功后清除退避标记；失败后记录退避（见 [`Self::backoff`]）并置 `connected = false`，
    ///   **下一次调用即自动重连**（rumqttc 语义：`network` 置空 + `pending` 保留）。
    ///
    /// # Errors
    /// 连接 / 协议错误按域收敛（见 [`map_connection_error`]）。
    pub async fn poll_event(&mut self) -> DaemonResult<Event> {
        match self.eventloop.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(ack))) => {
                if ack.code != ConnectReturnCode::Success {
                    self.connected = false;
                    return Err(DaemonError::SecurityError(format!(
                        "broker refused connection: {:?}",
                        ack.code
                    )));
                }
                self.session_present = ack.session_present;
                self.connected = true;
                self.connect_count += 1;
                self.retry_after = None;
                self.reconnector.reset();
                Ok(Event::Incoming(Packet::ConnAck(ack)))
            }
            Ok(event) => {
                // 慢消费者记账（task 54 接线点 1）：收到确认 → 未确认积压减一；
                // 若该报文出自发送队列，则从其在途窗口回收（**只有确认到达才回收**；
                // 未确认条目继续占用水位，因此不会被重复发布）。
                if matches!(
                    &event,
                    Event::Incoming(Packet::PubAck(_)) | Event::Incoming(Packet::PubComp(_))
                ) {
                    self.acked = self.acked.saturating_add(1);
                    // 该报文的确认来源：`Some(true)` = 出自发送队列 → 回收其水位。
                    if self.ack_sources.pop_front() == Some(true) {
                        if let Some(outlet) = &self.outlet {
                            outlet.send().confirm(1);
                        }
                    }
                }
                Ok(event)
            }
            Err(err) => {
                self.connected = false;
                self.retry_after = Some(self.reconnector.next_delay());
                Err(map_connection_error(err))
            }
        }
    }

    /// 若上次 [`Self::poll_event`] 失败，等待退避间隔并清除标记；否则立即返回。
    ///
    /// 返回实际等待的时长（无待重连时为 `None`）。
    pub async fn backoff(&mut self) -> Option<Duration> {
        let delay = self.retry_after.take()?;
        tokio::time::sleep(delay).await;
        Some(delay)
    }
}

impl fmt::Debug for EndpointConfig {
    /// 手写 Debug：`password` 恒打码，其余字段照实输出（便于排障）。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EndpointConfig")
            .field("name", &self.name)
            .field("broker", &self.broker)
            .field("port", &self.port)
            .field("client_id", &self.client_id)
            .field("username", &self.username)
            .field(
                "password",
                &match &self.password {
                    Some(_) => "<redacted>",
                    None => "<none>",
                },
            )
            .field("topic_prefix", &self.topic_prefix)
            .field("qos", &self.qos)
            .field("encoding", &self.encoding)
            .field("clean_session", &self.clean_session)
            .field("keep_alive", &self.keep_alive)
            .field("send_high_water", &self.send_high_water)
            .field("tls", &self.tls.as_ref().map(|_| "<configured>"))
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for MqttClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MqttClient")
            .field("endpoint", &self.endpoint)
            .field("session_present", &self.session_present)
            .field("connected", &self.connected)
            .field("connect_count", &self.connect_count)
            .field("retry_after", &self.retry_after)
            .field("outstanding", &self.outstanding())
            .field("ack_tracked", &self.ack_sources.len())
            .field("outlet_attached", &self.outlet.is_some())
            .finish_non_exhaustive()
    }
}

/// rumqttc [`ConnectionError`] → daemon 错误域（连接/证书 7000、传输 6000、协议 5000）。
fn map_connection_error(err: ConnectionError) -> DaemonError {
    match err {
        ConnectionError::Io(e) => DaemonError::NetworkError(format!("mqtt io: {e}")),
        ConnectionError::NetworkTimeout => {
            DaemonError::NetworkError("mqtt network timeout".to_string())
        }
        ConnectionError::FlushTimeout => {
            DaemonError::NetworkError("mqtt flush timeout".to_string())
        }
        ConnectionError::Tls(e) => DaemonError::SecurityError(format!("mqtt tls: {e}")),
        ConnectionError::ConnectionRefused(code) => match code {
            ConnectReturnCode::BadUserNamePassword | ConnectReturnCode::NotAuthorized => {
                DaemonError::SecurityError(format!("mqtt broker refused credentials: {code:?}"))
            }
            other => DaemonError::MqttError(format!("mqtt broker refused connection: {other:?}")),
        },
        other => DaemonError::MqttError(format!("mqtt connection: {other}")),
    }
}

// ---- 多路连接池 ----

/// 多路北向连接池（多 Broker 支持）：按出口名路由发布与事件推进。
///
/// 每路连接独立持有 [`MqttClient`]（独立 broker / client_id / qos / encoding / tls）。
#[derive(Default)]
pub struct MqttConnectionPool {
    endpoints: std::collections::HashMap<String, MqttClient>,
}

impl MqttConnectionPool {
    /// 空连接池。
    pub fn new() -> Self {
        Self {
            endpoints: std::collections::HashMap::new(),
        }
    }

    /// 注册一路连接（重名即 [`DaemonError::ConfigError`]）。
    ///
    /// # Errors
    /// 重名或配置非法 → `ConfigError`；证书问题 → `SecurityError`。
    pub fn register(&mut self, endpoint: EndpointConfig) -> DaemonResult<()> {
        if self.endpoints.contains_key(&endpoint.name) {
            return Err(DaemonError::ConfigError(format!(
                "duplicate outlet name `{}`",
                endpoint.name
            )));
        }
        let name = endpoint.name.clone();
        let client = MqttClient::new(endpoint)?;
        self.endpoints.insert(name, client);
        Ok(())
    }

    /// 注销一路连接（返回被移除的客户端）。
    pub fn remove(&mut self, name: &str) -> Option<MqttClient> {
        self.endpoints.remove(name)
    }

    /// 已注册路数。
    pub fn len(&self) -> usize {
        self.endpoints.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.endpoints.is_empty()
    }

    /// 是否已注册该出口名。
    pub fn contains(&self, name: &str) -> bool {
        self.endpoints.contains_key(name)
    }

    /// 全部出口名（字典序，日志与诊断稳定输出）。
    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.endpoints.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }

    /// 取一路连接的只读引用。
    ///
    /// # Errors
    /// 未注册 → [`DaemonError::ConfigError`]。
    pub fn get(&self, name: &str) -> DaemonResult<&MqttClient> {
        self.endpoints
            .get(name)
            .ok_or_else(|| DaemonError::ConfigError(format!("unknown outlet `{name}`")))
    }

    /// 取一路连接的可变引用（用于推进事件循环）。
    ///
    /// # Errors
    /// 未注册 → [`DaemonError::ConfigError`]。
    pub fn get_mut(&mut self, name: &str) -> DaemonResult<&mut MqttClient> {
        self.endpoints
            .get_mut(name)
            .ok_or_else(|| DaemonError::ConfigError(format!("unknown outlet `{name}`")))
    }

    /// 该路连接声明的载荷编码。
    ///
    /// # Errors
    /// 未注册 → [`DaemonError::ConfigError`]。
    pub fn encoding_of(&self, name: &str) -> DaemonResult<Encoding> {
        Ok(self.get(name)?.encoding())
    }

    /// 向指定出口发布一条原始报文（按该路默认 QoS）。
    ///
    /// # Errors
    /// 未注册 → `ConfigError`；发布失败 → `MqttError`。
    pub async fn publish(
        &self,
        name: &str,
        topic: &str,
        payload: impl Into<Vec<u8>>,
    ) -> DaemonResult<()> {
        self.get(name)?.publish(topic, payload).await
    }

    /// 向指定出口发布一个 [`ProcessedSample`]（编码器由调用方注入）。
    ///
    /// # Errors
    /// 未注册或编码不匹配 → `ConfigError`；序列化/发布失败 → 对应错误域。
    pub async fn publish_sample(
        &self,
        name: &str,
        sample: &ProcessedSample,
        encoder: &dyn PayloadEncoder,
    ) -> DaemonResult<()> {
        self.get(name)?.publish_sample(sample, encoder).await
    }

    /// 推进指定出口的一轮事件循环。
    ///
    /// # Errors
    /// 未注册 → `ConfigError`；连接/协议错误 → 对应错误域。
    pub async fn poll_event(&mut self, name: &str) -> DaemonResult<Event> {
        self.get_mut(name)?.poll_event().await
    }
}

impl fmt::Debug for MqttConnectionPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MqttConnectionPool")
            .field("names", &self.names())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use crate::backpressure::{DedupOutcome, DedupReason, DEFAULT_SEND_HARD_LIMIT};
    use crate::error::{ERR_CONFIG, ERR_MQTT, ERR_NETWORK, ERR_SECURITY};
    use crate::offline_queue::{AckSink, ManualClock, OfflineQueue, QueueConfig, QueueHooks};

    // ---- 模拟 Broker（仅本地回环，不依赖外网） ----

    /// Broker 观测到的一条 PUBLISH。
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct ObservedPublish {
        qos: u8,
        pkid: u16,
        topic: String,
        payload: Vec<u8>,
    }

    /// Broker 共享状态。
    #[derive(Debug, Default)]
    struct BrokerState {
        /// 持久会话登记表（client_id 集合）。
        sessions: HashSet<String>,
        publishes: Vec<ObservedPublish>,
        connections: usize,
        /// 已被主动断开的连接数（用于「只断前 N 次」）。
        dropped_connacks: usize,
    }

    /// 模拟 Broker 的故障注入选项（默认全 0 = 正常 Broker）。
    #[derive(Debug, Clone, Copy, Default)]
    struct BrokerOptions {
        /// 前 N 次连接在回复 CONNACK 后立即断开（模拟 Broker 主动断开）。
        drop_connack_times: usize,
        /// 发出第 N 个 PUBACK 后立即断开（0 = 不断开）。
        drop_after_pubacks: usize,
    }

    /// 原始 MQTT 报文（自行解析固定头，避免为单测引入额外依赖）。
    #[derive(Debug, Clone)]
    struct RawPacket {
        kind: u8,
        flags: u8,
        body: Vec<u8>,
    }

    fn encode_remaining_len(mut len: usize) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let mut byte = (len % 128) as u8;
            len /= 128;
            if len > 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if len == 0 {
                break;
            }
        }
        out
    }

    async fn read_packet(stream: &mut TcpStream) -> std::io::Result<Option<RawPacket>> {
        let mut first = [0u8; 1];
        match stream.read_exact(&mut first).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let kind = first[0] >> 4;
        let flags = first[0] & 0x0f;

        let mut len: usize = 0;
        let mut multiplier: usize = 1;
        loop {
            let mut byte = [0u8; 1];
            stream.read_exact(&mut byte).await?;
            len += ((byte[0] & 0x7f) as usize) * multiplier;
            if byte[0] & 0x80 == 0 {
                break;
            }
            multiplier = multiplier.saturating_mul(128);
        }

        let mut body = vec![0u8; len];
        if len > 0 {
            stream.read_exact(&mut body).await?;
        }
        Ok(Some(RawPacket { kind, flags, body }))
    }

    async fn write_packet(stream: &mut TcpStream, byte1: u8, body: &[u8]) -> std::io::Result<()> {
        let mut out = Vec::with_capacity(1 + 4 + body.len());
        out.push(byte1);
        out.extend_from_slice(&encode_remaining_len(body.len()));
        out.extend_from_slice(body);
        stream.write_all(&out).await?;
        stream.flush().await
    }

    /// 解析 CONNECT 的 client_id（MQTT 3.1.1 变长头布局固定）。
    fn parse_client_id(body: &[u8]) -> Option<(String, bool)> {
        if body.len() < 12 {
            return None;
        }
        let clean_session = (body[7] & 0x02) != 0;
        let len = u16::from_be_bytes([body[10], body[11]]) as usize;
        if body.len() < 12 + len {
            return None;
        }
        String::from_utf8(body[12..12 + len].to_vec())
            .ok()
            .map(|id| (id, clean_session))
    }

    fn parse_publish(flags: u8, body: &[u8]) -> Option<ObservedPublish> {
        if body.len() < 2 {
            return None;
        }
        let topic_len = u16::from_be_bytes([body[0], body[1]]) as usize;
        let mut offset = 2 + topic_len;
        if body.len() < offset {
            return None;
        }
        let topic = String::from_utf8(body[2..2 + topic_len].to_vec()).ok()?;
        let qos = (flags >> 1) & 0x03;
        let mut pkid = 0u16;
        if qos > 0 {
            if body.len() < offset + 2 {
                return None;
            }
            pkid = u16::from_be_bytes([body[offset], body[offset + 1]]);
            offset += 2;
        }
        Some(ObservedPublish {
            qos,
            pkid,
            topic,
            payload: body[offset..].to_vec(),
        })
    }

    /// 解析 SUBSCRIBE → (pkid, 每个过滤器的授予 QoS)。
    fn parse_subscribe(body: &[u8]) -> Option<(u16, Vec<u8>)> {
        if body.len() < 5 {
            return None;
        }
        let pkid = u16::from_be_bytes([body[0], body[1]]);
        let mut offset = 2;
        let mut codes = Vec::new();
        while offset + 2 < body.len() {
            let len = u16::from_be_bytes([body[offset], body[offset + 1]]) as usize;
            if offset + 2 + len >= body.len() {
                break;
            }
            codes.push(body[offset + 2 + len].min(2));
            offset += 3 + len;
        }
        if codes.is_empty() {
            codes.push(0x80);
        }
        Some((pkid, codes))
    }

    async fn handle_conn(
        stream: &mut TcpStream,
        state: Arc<Mutex<BrokerState>>,
        opts: BrokerOptions,
    ) {
        {
            let mut guard = state.lock().expect("broker state lock");
            guard.connections += 1;
        }

        let mut pubacks_sent = 0usize;
        while let Ok(Some(packet)) = read_packet(stream).await {
            match packet.kind {
                // CONNECT
                1 => {
                    let Some((client_id, clean_session)) = parse_client_id(&packet.body) else {
                        break;
                    };
                    let session_present = {
                        let mut guard = state.lock().expect("broker state lock");
                        if clean_session {
                            guard.sessions.remove(&client_id);
                            false
                        } else {
                            // insert 返回 true 表示首次登记 → session_present = false
                            !guard.sessions.insert(client_id)
                        }
                    };
                    if write_packet(stream, 0x20, &[u8::from(session_present), 0x00])
                        .await
                        .is_err()
                    {
                        break;
                    }
                    // 只断开前 `drop_connack_times` 次连接，之后保持在线（避免无限重连循环）。
                    let drop_now = {
                        let mut guard = state.lock().expect("broker state lock");
                        if guard.dropped_connacks < opts.drop_connack_times {
                            guard.dropped_connacks += 1;
                            true
                        } else {
                            false
                        }
                    };
                    if drop_now {
                        break;
                    }
                }
                // PUBLISH
                3 => {
                    let Some(publish) = parse_publish(packet.flags, &packet.body) else {
                        break;
                    };
                    state
                        .lock()
                        .expect("broker state lock")
                        .publishes
                        .push(publish.clone());
                    match publish.qos {
                        1 => {
                            let pkid = publish.pkid.to_be_bytes();
                            if write_packet(stream, 0x40, &pkid).await.is_err() {
                                break;
                            }
                            pubacks_sent += 1;
                            if opts.drop_after_pubacks > 0
                                && pubacks_sent >= opts.drop_after_pubacks
                            {
                                break;
                            }
                        }
                        2 => {
                            let pkid = publish.pkid.to_be_bytes();
                            if write_packet(stream, 0x50, &pkid).await.is_err() {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                // PUBREL
                6 => {
                    if packet.body.len() < 2 {
                        break;
                    }
                    if write_packet(stream, 0x70, &packet.body[0..2])
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                // SUBSCRIBE
                8 => {
                    let Some((pkid, codes)) = parse_subscribe(&packet.body) else {
                        break;
                    };
                    let mut body = pkid.to_be_bytes().to_vec();
                    body.extend_from_slice(&codes);
                    if write_packet(stream, 0x90, &body).await.is_err() {
                        break;
                    }
                }
                // PINGREQ
                12 => {
                    if write_packet(stream, 0xd0, &[]).await.is_err() {
                        break;
                    }
                }
                // DISCONNECT
                14 => break,
                _ => {}
            }
        }
    }

    struct MockBroker {
        port: u16,
        state: Arc<Mutex<BrokerState>>,
    }

    impl MockBroker {
        fn publishes(&self) -> Vec<ObservedPublish> {
            self.state
                .lock()
                .expect("broker state lock")
                .publishes
                .clone()
        }

        fn connections(&self) -> usize {
            self.state.lock().expect("broker state lock").connections
        }
    }

    /// 在 127.0.0.1 随机端口启动模拟 Broker（测试专用，不连外网）。
    async fn spawn_mock_broker(opts: BrokerOptions) -> MockBroker {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        let state = Arc::new(Mutex::new(BrokerState::default()));

        let shared = Arc::clone(&state);
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let state = Arc::clone(&shared);
                tokio::spawn(async move { handle_conn(&mut stream, state, opts).await });
            }
        });

        MockBroker { port, state }
    }

    /// 测试用 endpoint（回环 broker；keep_alive 取 30s，避免单测等 keepalive 触发）。
    fn endpoint_for(port: u16, client_id: &str) -> EndpointConfig {
        EndpointConfig::new("north-1", "127.0.0.1", port)
            .with_client_id(client_id)
            .with_keep_alive(Duration::from_secs(30))
            .with_reconnect(Duration::from_millis(5), Duration::from_millis(50), 2)
    }

    /// 推进事件循环直到 `pred` 命中（每轮带超时，避免 keepalive 空等）。
    async fn drive_until<F>(
        client: &mut MqttClient,
        rounds: usize,
        mut pred: F,
    ) -> DaemonResult<Event>
    where
        F: FnMut(&Event) -> bool,
    {
        for _ in 0..rounds {
            let event = tokio::time::timeout(Duration::from_secs(3), client.poll_event())
                .await
                .map_err(|_| DaemonError::NetworkError("poll timeout".to_string()))??;
            if pred(&event) {
                return Ok(event);
            }
        }
        Err(DaemonError::MqttError(
            "condition not met within poll budget".to_string(),
        ))
    }

    /// 连续推进若干轮，收集事件与错误；`done` 命中即提前返回（供重连类场景断言序列）。
    async fn drive_until_state<F>(
        client: &mut MqttClient,
        rounds: usize,
        idle_timeout: Duration,
        mut done: F,
    ) -> (Vec<Event>, Vec<DaemonError>)
    where
        F: FnMut(&[Event], &[DaemonError]) -> bool,
    {
        let mut events = Vec::new();
        let mut errors = Vec::new();
        for _ in 0..rounds {
            if done(&events, &errors) {
                break;
            }
            match tokio::time::timeout(idle_timeout, client.poll_event()).await {
                Ok(Ok(event)) => events.push(event),
                Ok(Err(err)) => errors.push(err),
                // 连续 3s 无任何事件 → 判定为空闲，提前结束（不空等 keepalive）。
                Err(_) => break,
            }
        }
        (events, errors)
    }

    fn connack_sessions(events: &[Event]) -> Vec<bool> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Incoming(Packet::ConnAck(ack)) => Some(ack.session_present),
                _ => None,
            })
            .collect()
    }

    fn incoming_pubacks(events: &[Event]) -> Vec<u16> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Incoming(Packet::PubAck(ack)) => Some(ack.pkid),
                _ => None,
            })
            .collect()
    }

    // ---- 配置校验（构建期暴露，错误码 2000） ----

    /// QA: 空 broker → ConfigError。
    #[test]
    fn empty_broker_is_rejected() {
        let err = MqttClient::new(EndpointConfig::new("o", "", 1883)).expect_err("empty broker");
        assert_eq!(err.error_code(), ERR_CONFIG);
    }

    /// QA: 空 client_id → ConfigError（持久会话不可能成立）。
    #[test]
    fn empty_client_id_is_rejected() {
        let endpoint = EndpointConfig::new("o", "127.0.0.1", 1883).with_client_id("");
        let err = MqttClient::new(endpoint).expect_err("empty client_id");
        assert_eq!(err.error_code(), ERR_CONFIG);
    }

    /// QA: port 0 → ConfigError。
    #[test]
    fn zero_port_is_rejected() {
        let err = MqttClient::new(EndpointConfig::new("o", "127.0.0.1", 0)).expect_err("zero port");
        assert_eq!(err.error_code(), ERR_CONFIG);
    }

    /// QA: keep_alive 处于 (0, 1s) 区间 → ConfigError（rumqttc 会 assert!，此处前置拦截）。
    #[test]
    fn sub_second_keep_alive_is_rejected() {
        let endpoint =
            EndpointConfig::new("o", "127.0.0.1", 1883).with_keep_alive(Duration::from_millis(500));
        let err = MqttClient::new(endpoint).expect_err("sub-second keep alive");
        assert_eq!(err.error_code(), ERR_CONFIG);
    }

    /// QA: 有口令无用户名 → ConfigError。
    #[test]
    fn password_without_username_is_rejected() {
        let mut endpoint = EndpointConfig::new("o", "127.0.0.1", 1883);
        endpoint.password = Some("TEST_ONLY_empty_username".to_string());
        let err = MqttClient::new(endpoint).expect_err("orphan password");
        assert_eq!(err.error_code(), ERR_CONFIG);
    }

    /// QA: 主题前缀含通配符 → ConfigError。
    #[test]
    fn wildcard_topic_prefix_is_rejected() {
        let endpoint = EndpointConfig::new("o", "127.0.0.1", 1883).with_topic_prefix("telemetry/#");
        let err = MqttClient::new(endpoint).expect_err("wildcard prefix");
        assert_eq!(err.error_code(), ERR_CONFIG);
    }

    /// QA: TLS 无自定义 CA = 操作系统根证书库（**合法**，2026-09-25 用户决策：
    /// 证书配置可选）；mTLS 只有半套 → ConfigError（证书域自检在构建期完成）。
    #[test]
    fn tls_without_ca_is_valid_but_half_mtls_is_rejected() {
        let system_roots = TlsConfig::default();
        system_roots
            .validate()
            .expect("no custom CA = OS root store (valid config)");

        let half_mtls = TlsConfig {
            ca_cert_path: Some(PathBuf::from("ca.pem")),
            client_cert_path: Some(PathBuf::from("client.pem")),
            client_key_path: None,
            alpn: Vec::new(),
        };
        assert_eq!(
            half_mtls.validate().expect_err("missing key").error_code(),
            ERR_CONFIG
        );
    }

    /// QA: 非法 QoS（3）→ ConfigError；0/1/2 可往返映射。
    #[test]
    fn qos_mapping_roundtrip_and_range_check() {
        assert_eq!(qos_to_u8(qos_from_u8(0).expect("qos 0")), 0);
        assert_eq!(qos_to_u8(qos_from_u8(1).expect("qos 1")), 1);
        assert_eq!(qos_to_u8(qos_from_u8(2).expect("qos 2")), 2);
        assert_eq!(qos_from_u8(3).expect_err("qos 3").error_code(), ERR_CONFIG);
        assert_eq!(
            EndpointConfig::new("o", "h", 1883)
                .with_qos(9)
                .expect_err("qos 9")
                .error_code(),
            ERR_CONFIG
        );
    }

    // ---- 编码声明（每路独立） ----

    /// QA: 每路连接可独立声明 encoding（默认 protobuf，可显式改 json）。
    #[test]
    fn encoding_is_declared_per_endpoint() {
        let mut pool = MqttConnectionPool::new();
        pool.register(
            EndpointConfig::new("north-a", "127.0.0.1", DEFAULT_MQTT_PORT).with_client_id("gw-a"),
        )
        .expect("register north-a");
        pool.register(
            EndpointConfig::new("north-b", "127.0.0.1", DEFAULT_MQTT_PORT)
                .with_client_id("gw-b")
                .with_encoding(Encoding::Json),
        )
        .expect("register north-b");

        assert_eq!(
            pool.encoding_of("north-a").expect("a"),
            Encoding::Protobuf,
            "未显式声明时应为默认 protobuf"
        );
        assert_eq!(
            pool.encoding_of("north-b").expect("b"),
            Encoding::Json,
            "每路独立声明 json 不应影响另一路"
        );
    }

    /// QA: 编码字面量解析（大小写不敏感 + proto 别名 + 非法值报错）。
    #[test]
    fn encoding_parse_accepts_config_literals() {
        assert_eq!(Encoding::parse("protobuf").expect("pb"), Encoding::Protobuf);
        assert_eq!(Encoding::parse("PROTO").expect("proto"), Encoding::Protobuf);
        assert_eq!(Encoding::parse(" JSON ").expect("json"), Encoding::Json);
        assert_eq!(
            Encoding::parse("cbor").expect_err("cbor").error_code(),
            ERR_CONFIG
        );
        assert_eq!(Encoding::Protobuf.as_str(), "protobuf");
        assert_eq!(Encoding::Json.to_string(), "json");
    }

    /// QA: 配置层 `OutletEncoding` 可无损转换为北向 `Encoding`（单一转换点）。
    #[test]
    fn config_outlet_encoding_converts_into_north_encoding() {
        assert_eq!(
            Encoding::from(crate::config::OutletEncoding::Json),
            Encoding::Json
        );
        assert_eq!(
            Encoding::from(crate::config::OutletEncoding::Protobuf),
            Encoding::Protobuf
        );
    }

    /// QA: 重名注册 → ConfigError；未注册出口 → ConfigError。
    #[test]
    fn pool_rejects_duplicate_and_unknown_names() {
        let mut pool = MqttConnectionPool::new();
        pool.register(EndpointConfig::new("dup", "127.0.0.1", 1883))
            .expect("first");
        assert_eq!(
            pool.register(EndpointConfig::new("dup", "127.0.0.1", 1883))
                .expect_err("duplicate")
                .error_code(),
            ERR_CONFIG
        );
        assert_eq!(
            pool.encoding_of("ghost").expect_err("unknown").error_code(),
            ERR_CONFIG
        );
        assert_eq!(pool.len(), 1);
        assert!(!pool.is_empty());
        assert!(pool.contains("dup"));
        assert_eq!(pool.names(), vec!["dup"]);
    }

    // ---- 发布 / 订阅（QA happy path） ----

    /// QA Happy: QoS 1 发布 → 收到 PUBACK，且 broker 侧收到完整 topic/payload。
    #[tokio::test]
    async fn publish_qos1_receives_puback() {
        let broker = spawn_mock_broker(BrokerOptions::default()).await;
        let mut client =
            MqttClient::new(endpoint_for(broker.port, "iot-daq-qos1")).expect("client");

        let connack = client.poll_event().await.expect("first poll connects");
        assert!(
            matches!(connack, Event::Incoming(Packet::ConnAck(_))),
            "首个事件应为 ConnAck，实际 {connack:?}"
        );
        assert!(client.is_connected());

        client
            .publish("telemetry/dev-1/p1", b"payload-qos1".to_vec())
            .await
            .expect("publish");

        // 先断言出站的 PUBLISH 带上了 pkid（QoS1 必然分配报文标识符）。
        let outgoing = drive_until(
            &mut client,
            8,
            |event| matches!(event, Event::Outgoing(rumqttc::Outgoing::Publish(pkid)) if *pkid > 0),
        )
        .await
        .expect("outgoing publish");
        assert!(
            matches!(outgoing, Event::Outgoing(rumqttc::Outgoing::Publish(pkid)) if pkid == 1),
            "QoS1 首条 PUBLISH 的 pkid 应为 1，实际 {outgoing:?}"
        );

        // 再断言收到 PUBACK。
        let puback = drive_until(&mut client, 8, |event| {
            matches!(event, Event::Incoming(Packet::PubAck(_)))
        })
        .await
        .expect("puback");
        assert!(
            matches!(puback, Event::Incoming(Packet::PubAck(ref ack)) if ack.pkid == 1),
            "PUBACK 的 pkid 应为 1，实际 {puback:?}"
        );

        let observed = broker.publishes();
        assert_eq!(observed.len(), 1, "broker 应只收到一条 PUBLISH");
        assert_eq!(observed[0].qos, 1, "broker 侧应观测到 QoS 1");
        assert_eq!(observed[0].topic, "telemetry/dev-1/p1");
        assert_eq!(observed[0].payload, b"payload-qos1".to_vec());
    }

    /// QA: QoS 0 发布不分配 pkid（`Outgoing::Publish(0)`），broker 侧不回 PUBACK。
    #[tokio::test]
    async fn publish_qos0_uses_pkid_zero() {
        let broker = spawn_mock_broker(BrokerOptions::default()).await;
        let mut client = MqttClient::new(
            endpoint_for(broker.port, "iot-daq-qos0")
                .with_qos(0)
                .expect("qos 0"),
        )
        .expect("client");

        client.poll_event().await.expect("connect");
        client
            .publish("telemetry/dev-1/p0", b"payload-qos0".to_vec())
            .await
            .expect("publish");

        let outgoing = drive_until(&mut client, 8, |event| {
            matches!(event, Event::Outgoing(rumqttc::Outgoing::Publish(_)))
        })
        .await
        .expect("outgoing publish");
        assert!(
            matches!(outgoing, Event::Outgoing(rumqttc::Outgoing::Publish(0))),
            "QoS0 的 PUBLISH pkid 应为 0，实际 {outgoing:?}"
        );

        // QoS0 不应有任何后续确认：短空闲窗口（300ms）内若仍无事件即判定「无 PUBACK」。
        let (events, _) = drive_until_state(
            &mut client,
            2,
            Duration::from_millis(300),
            |events, errors| !incoming_pubacks(events).is_empty() || !errors.is_empty(),
        )
        .await;
        assert!(incoming_pubacks(&events).is_empty(), "QoS0 不应产生 PUBACK");
        assert_eq!(broker.publishes()[0].qos, 0);
    }

    /// QA: 订阅往返 → 收到 SUBACK。
    #[tokio::test]
    async fn subscribe_receives_suback() {
        let broker = spawn_mock_broker(BrokerOptions::default()).await;
        let mut client = MqttClient::new(endpoint_for(broker.port, "iot-daq-sub")).expect("client");

        client.poll_event().await.expect("connect");
        client
            .subscribe("telemetry/cmd/#", QoS::AtLeastOnce)
            .await
            .expect("subscribe");

        let suback = drive_until(&mut client, 8, |event| {
            matches!(event, Event::Incoming(Packet::SubAck(_)))
        })
        .await
        .expect("suback");
        let suback_pkid = match &suback {
            Event::Incoming(Packet::SubAck(ack)) => ack.pkid,
            other => panic!("期望 SUBACK，实际 {other:?}"),
        };
        assert_eq!(suback_pkid, 1, "SUBACK pkid 应为 1");
    }

    // ---- 断线重连 + 会话恢复（QA error path） ----

    /// QA Error: Broker 主动断开 → 自动重连 + 会话恢复（CONNACK `session_present = true`），
    /// 且断线期间入队的 QoS1 报文在重连后被投递。
    #[tokio::test]
    async fn broker_disconnect_triggers_reconnect_and_session_resume() {
        let broker = spawn_mock_broker(BrokerOptions {
            drop_connack_times: 1,
            drop_after_pubacks: 0,
        })
        .await;

        let endpoint = endpoint_for(broker.port, "iot-daq-resume").with_clean_session(false);
        let mut client = MqttClient::new(endpoint).expect("client");

        // 首次连接：broker 侧尚无会话 → session_present = false。
        let first = client.poll_event().await.expect("first connack");
        assert!(
            matches!(first, Event::Incoming(Packet::ConnAck(ref ack)) if !ack.session_present),
            "首次连接 session_present 应为 false，实际 {first:?}"
        );

        // Broker 已断开：下一次 poll 应感知并报错，同时置位退避。
        let err = client.poll_event().await.expect_err("disconnect detected");
        assert!(
            matches!(err.error_code(), ERR_NETWORK | ERR_MQTT | ERR_SECURITY),
            "断线错误应收敛到网络/协议/安全域，实际 {} {:?}",
            err.error_code(),
            err
        );
        assert!(!client.is_connected(), "断线后不应处于已连接状态");
        assert!(client.retry_after().is_some(), "断线后应置位退避间隔");

        // 断线期间入队一条 QoS1 报文（session_present=true 时 rumqttc 不清空 pending）。
        client
            .publish("telemetry/dev-1/offline", b"queued-while-offline".to_vec())
            .await
            .expect("publish while offline");

        // 继续推进事件循环：自动重连 → 会话恢复 → 投递队列报文 → PUBACK。
        let (events, errors) =
            drive_until_state(&mut client, 16, Duration::from_secs(3), |events, _| {
                connack_sessions(events).len() == 1 && incoming_pubacks(events).contains(&1)
            })
            .await;

        let sessions = connack_sessions(&events);
        assert_eq!(
            sessions,
            vec![true],
            "重连后 CONNACK 应为 session_present=true（会话恢复），实际 {sessions:?}"
        );
        assert!(client.session_present(), "客户端应记录会话已恢复");
        assert!(
            client.reconnect_count() >= 1,
            "应至少完成一次自动重连，实际 {}",
            client.reconnect_count()
        );
        assert!(
            broker.connections() >= 2,
            "broker 应观测到至少 2 次连接，实际 {}",
            broker.connections()
        );
        assert!(
            incoming_pubacks(&events).contains(&1),
            "断线期间入队的 QoS1 报文应在重连后被确认，事件流 {events:?} / 错误 {errors:?}"
        );

        let observed = broker.publishes();
        assert!(
            observed.iter().any(|p| p.topic == "telemetry/dev-1/offline"
                && p.payload == b"queued-while-offline".to_vec()),
            "broker 应收到断线期间入队的报文，实际 {observed:?}"
        );
    }

    /// 对照实验：`clean_session = true` 时重连不恢复会话（session_present 恒为 false）。
    #[tokio::test]
    async fn clean_session_true_does_not_resume_session() {
        let broker = spawn_mock_broker(BrokerOptions {
            drop_connack_times: 1,
            drop_after_pubacks: 0,
        })
        .await;

        let endpoint = endpoint_for(broker.port, "iot-daq-clean").with_clean_session(true);
        let mut client = MqttClient::new(endpoint).expect("client");

        client.poll_event().await.expect("first connack");
        let _ = client.poll_event().await.expect_err("disconnect detected");

        let (events, _) =
            drive_until_state(&mut client, 12, Duration::from_secs(3), |events, _| {
                connack_sessions(events).len() == 1
            })
            .await;
        let sessions = connack_sessions(&events);
        assert_eq!(
            sessions,
            vec![false],
            "clean_session=true 时重连不应恢复会话，实际 {sessions:?}"
        );
    }

    // ---- 连接池路由（多 Broker） ----

    /// QA: 连接池按出口名路由到各自 Broker，互不串台。
    #[tokio::test]
    async fn pool_routes_publish_to_each_broker() {
        let broker_a = spawn_mock_broker(BrokerOptions::default()).await;
        let broker_b = spawn_mock_broker(BrokerOptions::default()).await;

        let mut pool = MqttConnectionPool::new();
        pool.register(endpoint_for(broker_a.port, "iot-daq-pool-a"))
            .expect("register a");
        pool.register(
            EndpointConfig::new("north-b", "127.0.0.1", broker_b.port)
                .with_client_id("iot-daq-pool-b")
                .with_keep_alive(Duration::from_secs(30))
                .with_encoding(Encoding::Json),
        )
        .expect("register b");
        assert_eq!(pool.names(), vec!["north-1", "north-b"]);

        pool.poll_event("north-1").await.expect("connect a");
        pool.poll_event("north-b").await.expect("connect b");

        pool.publish("north-1", "telemetry/a", b"to-a".to_vec())
            .await
            .expect("publish a");
        pool.publish("north-b", "telemetry/b", b"to-b".to_vec())
            .await
            .expect("publish b");

        for name in ["north-1", "north-b"] {
            drive_until(pool.get_mut(name).expect(name), 8, |event| {
                matches!(event, Event::Incoming(Packet::PubAck(_)))
            })
            .await
            .expect("puback");
        }

        assert_eq!(
            broker_a.publishes(),
            vec![ObservedPublish {
                qos: 1,
                pkid: 1,
                topic: "telemetry/a".to_string(),
                payload: b"to-a".to_vec(),
            }]
        );
        assert_eq!(
            broker_b.publishes(),
            vec![ObservedPublish {
                qos: 1,
                pkid: 1,
                topic: "telemetry/b".to_string(),
                payload: b"to-b".to_vec(),
            }]
        );
    }

    // ---- 载荷编码传递（契约，不实现序列化） ----

    /// 测试替身编码器（**不做真实序列化**，只按格式打标，用于验证 encoding 传递与校验）。
    struct StubEncoder {
        encoding: Encoding,
    }

    impl PayloadEncoder for StubEncoder {
        fn encoding(&self) -> Encoding {
            self.encoding
        }

        fn encode(&self, sample: &ProcessedSample) -> DaemonResult<Vec<u8>> {
            // 真实序列化 = task 62；此处仅产出可断言的占位字节。
            Ok(format!(
                "{}|{}|{}|{}",
                self.encoding.as_str(),
                sample.device_id,
                sample.point_id,
                sample.value
            )
            .into_bytes())
        }
    }

    fn sample_of(device_id: &str, point_id: &str, value: f64) -> ProcessedSample {
        ProcessedSample {
            device_id: device_id.to_string(),
            point_id: point_id.to_string(),
            value,
            unit: "kPa".to_string(),
            device_ts_ns: None,
            collected_ts_ns: 1_700_000_000_000_000_000,
            quality: protocol_proto::Quality::Good,
        }
    }

    /// QA: `publish_sample` 按 connection 的 encoding 校验编码器，主题按前缀拼接，
    /// 载荷原样透传（序列化细节不在本任务）。
    #[tokio::test]
    async fn publish_sample_checks_encoding_and_builds_topic() {
        let broker = spawn_mock_broker(BrokerOptions::default()).await;
        let mut client =
            MqttClient::new(endpoint_for(broker.port, "iot-daq-enc").with_encoding(Encoding::Json))
                .expect("client");
        client.poll_event().await.expect("connect");

        // 编码器声明与连接声明不一致 → ConfigError（2000），不发出报文。
        let mismatch = client
            .publish_sample(
                &sample_of("dev-1", "p1", 36.5),
                &StubEncoder {
                    encoding: Encoding::Protobuf,
                },
            )
            .await
            .expect_err("encoding mismatch");
        assert_eq!(mismatch.error_code(), ERR_CONFIG);

        // 一致 → 主题 = {prefix}/{device_id}/{point_id}，载荷由编码器产出。
        client
            .publish_sample(
                &sample_of("dev-1", "p1", 36.5),
                &StubEncoder {
                    encoding: Encoding::Json,
                },
            )
            .await
            .expect("publish sample");

        drive_until(&mut client, 8, |event| {
            matches!(event, Event::Incoming(Packet::PubAck(_)))
        })
        .await
        .expect("puback");

        let observed = broker.publishes();
        assert_eq!(observed.len(), 1, "编码不匹配时不应发出任何报文");
        assert_eq!(observed[0].topic, "telemetry/dev-1/p1");
        assert_eq!(observed[0].payload, b"json|dev-1|p1|36.5".to_vec());
    }

    // ---- TLS 配置映射 ----

    /// QA: TLS 证书路径 → rumqttc `Transport::Tls`（急切构建 `ClientConfig`，
    /// 用真实测试 CA 夹具）；缺失文件 → SecurityError（7000）。
    #[test]
    fn tls_config_maps_to_rustls_transport() {
        let dir = tempfile::tempdir().expect("tempdir");
        // 真实证书（自签名 ECDSA 测试夹具）：急切路径要求能解析进 RootCertStore。
        let real_ca = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tls/ca.crt");

        let config = TlsConfig::ca_only(&real_ca);
        // `Transport` 未实现 Debug，故用 match 断言变体（避免 Debug 格式化）。
        match config.to_transport() {
            Ok(Transport::Tls(_)) => {}
            Ok(_) => panic!("启用 TLS 时应映射到 Transport::Tls（实际为明文变体）"),
            Err(e) => panic!("TLS 传输构建失败: {e}"),
        }

        let broken = TlsConfig::ca_only(dir.path().join("missing.pem"));
        let err = broken.to_transport().err().expect("缺失 CA 应失败");
        assert_eq!(err.error_code(), ERR_SECURITY, "证书读取失败属安全域 7000");
    }

    /// QA: 非 PEM 内容（无 BEGIN 标记）→ SecurityError，不把解析 panic 留给 rustls。
    #[test]
    fn non_pem_ca_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ca = dir.path().join("ca.txt");
        std::fs::write(&ca, b"not a certificate").expect("write");
        let err = TlsConfig::ca_only(&ca)
            .to_transport()
            .err()
            .expect("non-pem ca");
        assert_eq!(err.error_code(), ERR_SECURITY);
    }

    /// QA: `ca_cert_path` 缺省 → 操作系统根证书库（用 `SSL_CERT_FILE` 指向测试 CA /
    /// 不存在的路径做**封闭**验证：非空信任锚成功、空信任锚 fail-closed SecurityError）。
    /// 正反两例放同一测试内**串行**执行，避免并行测试竞改进程级环境变量。
    #[test]
    fn system_root_store_fallback_is_hermetic() {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tls/ca.crt");
        assert!(
            fixture.is_file(),
            "test CA fixture missing: {}",
            fixture.display()
        );

        // 正例：SSL_CERT_FILE 指向测试 CA → 系统库非空 → Tls 传输构建成功。
        std::env::set_var("SSL_CERT_FILE", &fixture);
        let ok = TlsConfig::default().to_transport();
        std::env::remove_var("SSL_CERT_FILE");
        // `Transport` 未实现 Debug，用 match 断言变体。
        match ok {
            Ok(Transport::Tls(_)) => {}
            Ok(_) => panic!("启用 TLS 时应映射到 Transport::Tls（实际为明文变体）"),
            Err(e) => panic!("OS root store 传输构建失败: {e}"),
        }

        // 反例：信任锚为空 → fail-closed（绝不静默跳过校验）。
        std::env::set_var("SSL_CERT_FILE", "/nonexistent/iotdaq-test-no-such-ca.pem");
        // `Transport` 未实现 Debug，不能用 expect_err（要求 Ok 变体可 Debug）。
        let err = match TlsConfig::default().to_transport() {
            Ok(_) => panic!("empty trust anchor must fail"),
            Err(e) => e,
        };
        std::env::remove_var("SSL_CERT_FILE");
        assert_eq!(err.error_code(), ERR_SECURITY);
    }

    /// QA（守护性回归）：rustls 必须有可用的进程级 crypto provider，否则 mqtts 握手必然失败。
    ///
    /// **若此测试失败，说明 rustls 的 `ring`（或 `aws-lc-rs`）provider 特性被移除**——
    /// 典型场景是有人「精简」`crates/daemon/Cargo.toml` 时删掉了
    /// `rustls = { ..., features = ["std", "tls12", "ring"] }` 的 `ring`。
    /// 双重守护：
    /// - 编译期：`ensure_rustls_provider()` 引用 `rustls::crypto::ring`，特性消失即编译失败；
    /// - 运行期：本测试断言 `CryptoProvider::get_default().is_some()`。
    #[test]
    fn tls_crypto_provider_is_installed() {
        // 走一次真实构建路径（`to_transport` 内部会确保 provider 就位）。
        let real_ca = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tls/ca.crt");
        match TlsConfig::ca_only(&real_ca).to_transport() {
            Ok(Transport::Tls(_)) => {}
            Ok(_) => panic!("启用 TLS 时应映射到 Transport::Tls（实际为明文变体）"),
            Err(e) => panic!("TLS 传输构建失败: {e}"),
        }

        assert!(
            CryptoProvider::get_default().is_some(),
            "rustls 无进程级 CryptoProvider：ring/aws-lc-rs provider 特性被移除会导致 mqtts 握手失败"
        );
    }

    /// QA（守护性回归）：`ensure_rustls_provider()` 幂等——重复调用不得报错，
    /// 且不覆盖宿主已安装的 provider（此处即重复调用同一函数两次）。
    #[test]
    fn ensure_rustls_provider_is_idempotent() {
        ensure_rustls_provider().expect("first ensure");
        ensure_rustls_provider().expect("second ensure");
        assert!(CryptoProvider::get_default().is_some());
    }

    /// 取一个 `CryptoProvider` 的密码套件标识列表（`SupportedCipherSuite::suite()`）。
    ///
    /// 用于逐项比较不同 provider 的套件集合——顺序敏感，与 `CryptoProvider` 文档
    /// 承诺的「按优先级排序」一致。
    fn cipher_suite_ids(provider: &CryptoProvider) -> Vec<rustls::CipherSuite> {
        provider.cipher_suites.iter().map(|cs| cs.suite()).collect()
    }

    /// QA（依赖红线守护性回归）：**实际生效的进程级 CryptoProvider 必须是 `ring`，而非 `aws-lc-rs`**。
    ///
    /// 背景：`aws-lc-rs`（cmake/NASM C 构建）违反「纯 Rust 依赖栈」红线，已从 daemon
    /// 依赖图移除（`rumqttc` 改 `use-rustls-no-provider`、dev-dep `tokio-rustls` 关闭
    /// default 特性）。`cargo tree` 只能证明**编译期**依赖图干净；本测试从**运行期**实测
    /// 证明真正被安装并生效的 provider 是 `ring`。
    ///
    /// 断言方式：走 daemon 的 provider 安装路径（[`ensure_rustls_provider`]）后，
    /// 取进程级 provider 的密码套件列表，与**新建的 ring provider** 的密码套件列表
    /// **逐项相等**。若外部（宿主 / 误引入的 aws-lc-rs）塞进了不同 provider，两者的
    /// 套件集合/顺序必然不一致，本测试即失败。
    ///
    /// 非空性证明：末尾的「控制实验」故意篡改一份 ring provider 的套件列表，断言比较
    /// 逻辑能识别差异（`assert_ne!`）。若上面的 `assert_eq!` 是恒真空断言，该控制实验
    /// 也会一并失败——因此两者共同保证断言有效。
    #[test]
    fn runtime_provider_is_ring_not_aws_lc() {
        // 1) 走 daemon 真实安装路径（幂等；测试并发下可能被他用例先装，仍成立）。
        ensure_rustls_provider().expect("daemon provider install path must succeed");

        // 2) 进程级 provider 必须存在。
        let installed = CryptoProvider::get_default()
            .expect("daemon ensure_rustls_provider must leave a process-level CryptoProvider");

        // 3) 以「新建的 ring provider」为真值来源，逐项比较密码套件。
        let ring_provider = rustls::crypto::ring::default_provider();
        let installed_suites = cipher_suite_ids(installed);
        let ring_suites = cipher_suite_ids(&ring_provider);

        // 非空性：ring 默认 provider 必带套件，排除「两边都空 → 假通过」。
        assert!(
            !ring_suites.is_empty(),
            "ring 默认 provider 的密码套件列表不应为空（否则相等断言无意义）"
        );
        // 内容断言：包含一个 ring 必然支持的 TLS1.3 套件（进一步排除空/占位 provider）。
        assert!(
            installed_suites.contains(&rustls::CipherSuite::TLS13_AES_256_GCM_SHA384),
            "进程级 provider 缺少 TLS13_AES_256_GCM_SHA384 —— 生效 provider 疑似非 ring"
        );
        // 逐项相等：生效 provider 与 ring 完全一致 ⇒ 生效 provider 就是 ring。
        assert_eq!(
            installed_suites, ring_suites,
            "进程级 CryptoProvider 的密码套件与 ring 不一致 —— 实际生效的 provider 不是 ring \
             （疑似被 aws-lc-rs 等替换），违反依赖红线"
        );

        // ---- 控制实验：证明上面的断言「可失败」而非恒真 ----
        // 故意篡改一份 ring provider（去掉最高优先级套件），断言比较逻辑能识别差异。
        let mut tampered = rustls::crypto::ring::default_provider();
        let _ = tampered.cipher_suites.pop();
        assert_ne!(
            cipher_suite_ids(&tampered),
            ring_suites,
            "比较逻辑必须能识别 suite 列表差异；否则 provider 一致性断言无意义"
        );
    }

    // ---- 慢消费者保护（task 54：发送水位 + 落盘降级回调） ----

    /// 记录型降级回调（测试替身）：记录每次触发时的积压数。
    struct RecordingDegrade {
        calls: Mutex<Vec<usize>>,
    }

    impl RecordingDegrade {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<usize> {
            self.calls.lock().expect("degrade calls lock").clone()
        }
    }

    impl SlowConsumerSink for RecordingDegrade {
        fn on_slow_consumer(&self, outstanding: usize) {
            self.calls
                .lock()
                .expect("degrade calls lock")
                .push(outstanding);
        }
    }

    /// QA: 积压达到水位 → 触发降级回调、不再发送（broker 只见水位内的报文），
    /// 返回 `Degraded` 携带触发时的积压数。
    #[tokio::test]
    async fn watermark_triggers_degrade_callback_and_skips_send() {
        let broker = spawn_mock_broker(BrokerOptions::default()).await;
        let mut client =
            MqttClient::new(endpoint_for(broker.port, "iot-daq-wm").with_send_watermarks(64, 3))
                .expect("client");
        client.poll_event().await.expect("connect");

        // 水位以内：正常发送（不轮询确认 → outstanding 持续增长）。
        for i in 0..3 {
            let outcome = client
                .publish_backpressured(
                    &format!("telemetry/dev-1/p{i}"),
                    b"ok".to_vec(),
                    &RecordingDegrade::new(),
                )
                .await
                .expect("publish under watermark");
            assert_eq!(outcome, PublishOutcome::Sent);
        }
        assert_eq!(client.outstanding(), 3);

        // 第 4 条：积压 3 >= 水位 3 → 降级（未发送），回调收到积压数 3。
        let degrade = RecordingDegrade::new();
        let outcome = client
            .publish_backpressured("telemetry/dev-1/p3", b"degraded".to_vec(), &degrade)
            .await
            .expect("degraded publish must not error");
        assert_eq!(outcome, PublishOutcome::Degraded { outstanding: 3 });
        assert_eq!(degrade.calls(), vec![3], "降级回调必须收到触发时的积压数");
        assert_eq!(client.outstanding(), 3, "被降级的报文不计入积压");

        // 推进事件循环把水位内的 3 条投出去并等 broker 确认（PUBACK 保证
        // broker 侧已完成记录——Outgoing 事件不保证对端已处理）。
        let mut pubacks = 0;
        for _ in 0..24 {
            if pubacks >= 3 {
                break;
            }
            let event = tokio::time::timeout(Duration::from_secs(3), client.poll_event())
                .await
                .expect("poll timeout")
                .expect("poll error");
            if matches!(event, Event::Incoming(Packet::PubAck(_))) {
                pubacks += 1;
            }
        }
        assert_eq!(pubacks, 3, "水位内的 3 条必须全部投出并被确认");

        // broker 只见水位内的 3 条；被降级的「degraded」报文绝不出现在网络上。
        let observed = broker.publishes();
        assert_eq!(observed.len(), 3, "超水位的报文不得发往 broker");
        assert!(
            observed
                .iter()
                .all(|p| p.payload == b"ok".to_vec() && p.topic.starts_with("telemetry/dev-1/p")),
            "broker 只应收到水位内报文: {observed:?}"
        );
    }

    /// QA: 收到 PUBACK 后积压回落 → 水位恢复可用（降级是暂态，不是熔断）。
    #[tokio::test]
    async fn watermark_recovers_after_pubacks() {
        let broker = spawn_mock_broker(BrokerOptions::default()).await;
        let mut client = MqttClient::new(
            endpoint_for(broker.port, "iot-daq-wm-rec").with_send_watermarks(64, 1),
        )
        .expect("client");
        client.poll_event().await.expect("connect");

        let degrade = RecordingDegrade::new();
        // outstanding=0 < 水位 1 → Sent；outstanding=1 >= 水位 1 → Degraded。
        assert_eq!(
            client
                .publish_backpressured("telemetry/a", b"1".to_vec(), &degrade)
                .await
                .expect("first"),
            PublishOutcome::Sent
        );
        assert_eq!(
            client
                .publish_backpressured("telemetry/a", b"2".to_vec(), &degrade)
                .await
                .expect("second"),
            PublishOutcome::Degraded { outstanding: 1 }
        );
        assert_eq!(degrade.calls(), vec![1]);

        // 推进事件循环直到 PUBACK → 积压归零 → 再次可发送。
        drive_until(&mut client, 8, |event| {
            matches!(event, Event::Incoming(Packet::PubAck(_)))
        })
        .await
        .expect("puback");
        assert_eq!(client.outstanding(), 0, "PUBACK 后积压必须回落");
        assert_eq!(
            client
                .publish_backpressured("telemetry/a", b"3".to_vec(), &degrade)
                .await
                .expect("third"),
            PublishOutcome::Sent,
            "积压回落后水位必须恢复放行"
        );
        assert_eq!(degrade.calls(), vec![1], "第二次发送不得再触发降级");
    }

    /// QA: 降级路径零网络依赖、零阻塞——积压只靠「提交」累积（事件循环未推进、
    /// 连接未建立），水位判定 + 回调同步完成，绝不等待网络。
    #[tokio::test]
    async fn degrade_path_never_blocks_or_touches_network() {
        // 从未 poll（未建立任何连接）——publish 只是入请求通道（容量 64）。
        let mut client = MqttClient::new(
            endpoint_for(DEFAULT_MQTT_PORT, "iot-daq-wm-off").with_send_watermarks(64, 4),
        )
        .expect("client");
        let degrade = RecordingDegrade::new();

        for i in 0..4 {
            let outcome = client
                .publish_backpressured(&format!("telemetry/q/{i}"), b"x".to_vec(), &degrade)
                .await
                .expect("queued publish must not error without connection");
            assert_eq!(outcome, PublishOutcome::Sent, "水位内提交不得依赖连接");
        }
        assert_eq!(client.outstanding(), 4);
        assert_eq!(degrade.calls(), Vec::<usize>::new(), "水位内不得触发降级");

        // 第 5 条：超水位 → 同步降级（无连接、无阻塞、无错误）。
        let outcome = client
            .publish_backpressured("telemetry/q/4", b"y".to_vec(), &degrade)
            .await
            .expect("degraded publish");
        assert_eq!(outcome, PublishOutcome::Degraded { outstanding: 4 });
        assert_eq!(degrade.calls(), vec![4]);
        assert!(!client.is_connected(), "全程不得建立连接");
    }

    /// QA: 水位参数构建期校验——0 或超过通道容量 → ConfigError（2000）。
    #[test]
    fn send_watermark_above_channel_capacity_is_rejected() {
        let zero = EndpointConfig::new("o", "127.0.0.1", 1883).with_send_watermarks(64, 0);
        assert_eq!(
            zero.validate().expect_err("zero watermark").error_code(),
            ERR_CONFIG
        );
        let over = EndpointConfig::new("o", "127.0.0.1", 1883).with_send_watermarks(64, 65);
        assert_eq!(
            over.validate()
                .expect_err("watermark > capacity")
                .error_code(),
            ERR_CONFIG
        );
        let ok = EndpointConfig::new("o", "127.0.0.1", 1883).with_send_watermarks(64, 64);
        ok.validate().expect("watermark == capacity is allowed");
        assert_eq!(ok.send_high_water, 64);
        assert_eq!(DEFAULT_SEND_HIGH_WATER, 32, "默认水位决议值 32");
    }

    /// QA: `EndpointConfig` Debug 不得泄露口令（回归守卫，新增字段后仍需打码）。
    #[test]
    fn endpoint_config_debug_still_redacts_after_watermark_field() {
        const TEST_ONLY_PW: &str = "TEST_ONLY_WatermarkPw!";
        let mut endpoint = EndpointConfig::new("wm-leak", "broker.local", 1883);
        endpoint.password = Some(TEST_ONLY_PW.to_string());
        let dbg = format!("{endpoint:?}");
        assert!(!dbg.contains(TEST_ONLY_PW), "口令泄露: {dbg}");
        assert!(dbg.contains("send_high_water"), "水位字段应可观测: {dbg}");
    }

    /// QA: 主题拼接与地址文本（日志用，不含凭证）。
    #[test]
    fn topic_and_address_helpers() {
        let endpoint = EndpointConfig::new("o", "broker.local", DEFAULT_MQTTS_PORT)
            .with_topic_prefix("telemetry/v1");
        assert_eq!(endpoint.address(), "broker.local:8883");
        assert_eq!(
            endpoint.topic_for("dev-7", "PT-01"),
            "telemetry/v1/dev-7/PT-01"
        );
        assert_eq!(endpoint.broker, "broker.local");
    }

    // ---- 回归：QA 发现的「Debug 明文泄露口令」缺陷（Major） ----

    /// `EndpointConfig` 的 Debug 必须打码口令。
    ///
    /// QA 实测：`#[derive(Debug)]` 曾把 `password: Some("SUPER_SECRET_PASSWORD_123")`
    /// 原样打出来，口令会随任何 `{:?}`（日志 / panic 现场 / 诊断导出）泄露。
    #[test]
    fn endpoint_config_debug_redacts_password() {
        const TEST_ONLY_PASSWORD: &str = "TEST_ONLY_SuperSecret!Pw123";
        let mut endpoint = EndpointConfig::new("leak-check", "broker.local", 1883);
        endpoint.username = Some("operator".to_string());
        endpoint.password = Some(TEST_ONLY_PASSWORD.to_string());

        // 1) 口令绝不能出现在 Debug 输出里。
        let config_dbg = format!("{endpoint:?}");
        assert!(
            !config_dbg.contains(TEST_ONLY_PASSWORD),
            "EndpointConfig Debug 泄露口令: {config_dbg}"
        );
        assert!(
            config_dbg.contains("<redacted>"),
            "口令字段应打码为 <redacted>: {config_dbg}"
        );
        // 2) 其余可排障字段照常输出（用户名不敏感，保留）。
        assert!(
            config_dbg.contains("broker.local"),
            "broker 应保留: {config_dbg}"
        );
        assert!(
            config_dbg.contains("operator"),
            "username 应保留: {config_dbg}"
        );

        // 3) 无口令时明确标注 <none>，便于区分「未配置」与「已脱敏」。
        let mut anonymous = EndpointConfig::new("anon", "broker.local", 1883);
        anonymous.password = None;
        let anon_dbg = format!("{anonymous:?}");
        assert!(anon_dbg.contains("<none>"), "无口令应为 <none>: {anon_dbg}");
    }

    /// `TlsConfig` 路径不应在 Debug 里暴露私钥文件内容（只给 `<configured>`）。
    #[test]
    fn endpoint_config_debug_hides_tls_paths() {
        let mut endpoint = EndpointConfig::new("tls-check", "broker.local", 8883);
        endpoint.tls = Some(TlsConfig {
            ca_cert_path: Some(PathBuf::from("/etc/iotdaq/ca.pem")),
            client_cert_path: Some(PathBuf::from("/etc/iotdaq/client.pem")),
            client_key_path: Some(PathBuf::from("/etc/iotdaq/client.key")),
            alpn: Vec::new(),
        });
        let dbg = format!("{endpoint:?}");
        assert!(
            !dbg.contains("client.key"),
            "TLS 私钥路径不应出现在 Debug: {dbg}"
        );
        assert!(dbg.contains("<configured>"), "TLS 应标注已配置: {dbg}");
    }

    // ==================== 背压接线（task 54） ====================

    /// 记录型审计上报出口（接线点 3 的落点）。
    #[derive(Default)]
    struct RecordingAuditSink {
        events: Mutex<Vec<BackpressureAudit>>,
    }

    impl RecordingAuditSink {
        fn events(&self) -> Vec<BackpressureAudit> {
            self.events.lock().expect("audit sink lock").clone()
        }
    }

    impl AuditSink for RecordingAuditSink {
        fn emit(&self, events: Vec<BackpressureAudit>) {
            self.events.lock().expect("audit sink lock").extend(events);
        }
    }

    /// 必定失败的 Ack 持久化（模拟 Ack 丢失 / 落盘不可用）。
    struct FailingAckSink;

    impl AckSink for FailingAckSink {
        fn persist_ack(&self, seq: u64) -> DaemonResult<()> {
            Err(DaemonError::StorageError(format!(
                "injected ack persistence failure for batch_seq {seq}"
            )))
        }
    }

    /// 临时目录 + 真实离线队列（队列库文件名必须是 `queue.db`）。
    fn temp_queue(gateway: &str) -> (tempfile::TempDir, Arc<OfflineQueue>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = QueueConfig::new(dir.path().join("queue.db"), gateway).expect("queue cfg");
        let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(0));
        let queue = OfflineQueue::open(cfg, clock).expect("open offline queue");
        (dir, Arc::new(queue))
    }

    /// 建一个挂好背压接线束的客户端（**不建连**；审计出口由本函数创建）。
    fn client_with_outlet(
        port: u16,
        client_id: &str,
        gateway: &str,
        queue: &Arc<OfflineQueue>,
    ) -> (MqttClient, Arc<NorthOutlet>, Arc<RecordingAuditSink>) {
        let sink = Arc::new(RecordingAuditSink::default());
        let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(0));
        let outlet = Arc::new(NorthOutlet::new(
            gateway,
            Arc::clone(queue),
            clock,
            sink.clone(),
        ));
        let client = MqttClient::new(endpoint_for(port, client_id))
            .expect("client")
            .with_outlet(Arc::clone(&outlet));
        (client, outlet, sink)
    }

    /// 接线点 1（QA）：消费者人为变慢（**永不确认**）→ 内存必须**有界**、
    /// 超限**落盘降级**、**审计有记录**，且一条数据都不静默丢。
    #[test]
    fn slow_consumer_bounds_memory_spills_to_disk_and_audits() {
        const TOTAL: usize = 200;

        let (_dir, queue) = temp_queue("gw-slow");
        let audit = Arc::new(AuditLog::new(256));
        let sink = Arc::new(RecordingAuditSink::default());
        let pump = AuditPump::new(Arc::clone(&audit), sink.clone());
        let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(0));
        let send = NorthSendQueue::new(Arc::clone(&queue), Arc::clone(&audit), clock);

        // 消费者永不确认（既不 take_ready 也不 confirm）：连推 200 条（远超硬上限 64）。
        for seq in 1..=TOTAL as u64 {
            let _ = send.push(seq, vec![0xAB; 64]);
        }

        let stats = send.stats();
        assert!(
            send.pending() <= DEFAULT_SEND_HARD_LIMIT,
            "内存必须有界（水位口径 = ready + sent）：{stats:?}"
        );
        assert!(stats.spilled > 0, "超限必须落盘降级：{stats:?}");
        assert_eq!(
            stats.admitted + stats.spilled + stats.rejected,
            TOTAL as u64,
            "每条数据都必须被记账（绝不静默丢）：{stats:?}"
        );
        assert_eq!(
            stats.spilled,
            TOTAL as u64 - stats.admitted,
            "非入内存的条目必须全部走落盘降级：{stats:?}"
        );
        assert!(
            send.bytes() <= DEFAULT_SEND_HARD_LIMIT * 64,
            "在途字节必须有界：{}",
            send.bytes()
        );

        // 降级数据真的进了离线队列（内存 + 磁盘），而不是被丢弃。
        let queued = queue.pending().expect("queue pending");
        assert!(queued > 0, "降级数据必须落到 OfflineQueue，实际 {queued}");

        // 接线点 3：审计留痕 + 有真实「取走上报」调用点。
        assert!(
            audit.contains_kind("HighWaterEntered"),
            "必须记录高水位进入"
        );
        assert!(
            audit.contains_kind("SlowConsumerSpilled"),
            "必须记录慢消费者落盘降级"
        );
        let emitted = pump.pump();
        assert!(emitted >= 2, "审计必须有取走上报的调用点，实际 {emitted}");
        assert_eq!(pump.log().len(), 0, "drain 后审计环必须清空");
        assert!(
            sink.events()
                .iter()
                .any(|e| e.kind() == "SlowConsumerSpilled"),
            "审计必须真的上报到出口"
        );
    }

    /// 接线点 1（QA）：**PUBACK 未到**时条目仍留在 `sent` 在途窗口、**不得被重复取出发布**。
    #[test]
    fn unacked_items_stay_inflight_and_are_never_republished() {
        let (_dir, queue) = temp_queue("gw-inflight");
        let audit = Arc::new(AuditLog::default());
        let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(0));
        let send = NorthSendQueue::new(queue, audit, clock);

        for seq in 1..=5u64 {
            assert!(
                send.push(seq, vec![1, 2, 3]).is_admitted(),
                "水位内必须入内存队列"
            );
        }
        assert_eq!(send.pending(), 5);

        let ready = send.take_ready(5);
        assert_eq!(
            ready.iter().map(|i| i.seq).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5],
            "必须按提交顺序取回"
        );
        assert_eq!(send.pending(), 5, "take_ready 只搬队列，水位占用不变");

        // PUBACK 未到：再取一次必须为空（同一批不得被重复发布）。
        assert!(
            send.take_ready(5).is_empty(),
            "未确认条目不得被重复取出发布"
        );

        // PUBACK 到达 → 按提交顺序回收。
        assert_eq!(send.confirm(2), 2);
        assert_eq!(send.pending(), 3);
        assert_eq!(send.confirm(99), 3, "confirm 不得超过在途条数");
        assert_eq!(send.pending(), 0);
        assert_eq!(send.bytes(), 0);
    }

    /// 接线点 1（QA，真实链路）：PUBACK 到达前泵不重复发布；PUBACK 到达后水位回收，
    /// broker 侧条数不多不少。
    #[tokio::test]
    async fn puback_gates_pump_and_reclaims_watermark_on_the_wire() {
        let broker = spawn_mock_broker(BrokerOptions::default()).await;
        let (_dir, queue) = temp_queue("gw-net");
        let (mut client, _outlet, _sink) =
            client_with_outlet(broker.port, "iot-daq-pump1", "gw-net", &queue);
        client.poll_event().await.expect("connect");

        for seq in 1..=5u64 {
            let _ = client
                .submit(seq, format!("payload-{seq}").into_bytes())
                .expect("submit");
        }

        let first = client.pump_send("telemetry").await;
        assert_eq!(first.taken, 5);
        assert_eq!(first.published, 5);
        assert_eq!(first.requeued, 0);
        assert_eq!(
            client.queued_pending(),
            5,
            "PUBACK 未到 → 条目仍留在 sent 在途窗口"
        );

        // 再泵一次（PUBACK 仍未到）：不得重复发布。
        let second = client.pump_send("telemetry").await;
        assert_eq!(second.published, 0, "未确认条目不得重复发布");
        assert_eq!(client.queued_pending(), 5);

        // 驱动事件循环收齐 PUBACK → 水位回收。
        for _ in 0..64 {
            if client.queued_pending() == 0 {
                break;
            }
            let _ = tokio::time::timeout(Duration::from_secs(3), client.poll_event()).await;
        }
        assert_eq!(client.queued_pending(), 0, "PUBACK 后必须回收在途窗口");
        assert_eq!(client.outstanding(), 0);

        let observed = broker.publishes();
        assert_eq!(observed.len(), 5, "broker 侧不得出现重复发布：{observed:?}");
        let mut seen: Vec<String> = observed
            .iter()
            .filter_map(|p| String::from_utf8(p.payload.clone()).ok())
            .collect();
        seen.sort();
        assert_eq!(
            seen,
            vec![
                "payload-1".to_string(),
                "payload-2".to_string(),
                "payload-3".to_string(),
                "payload-4".to_string(),
                "payload-5".to_string()
            ]
        );
    }

    /// 接线点 2（QA）：补发**重复批次**时，幂等账本对同一 `batch_seq` 只 `Applied` 一次。
    #[test]
    fn duplicate_replay_batches_are_applied_at_most_once() {
        let (_dir, queue) = temp_queue("gw-replay");
        for i in 0..10 {
            queue
                .enqueue(format!("row-{i}").into_bytes())
                .expect("enqueue");
        }
        let audit = Arc::new(AuditLog::default());
        let replay = NorthReplay::new("gw-replay", Arc::clone(&queue), audit);

        let batch = replay.next_replay_batch(10).expect("replay batch");
        assert_eq!(batch.len(), 10);
        let mut applied = 0usize;
        for b in &batch {
            if replay.mark_sent(b.seq).is_applied() {
                applied += 1;
            }
        }
        assert_eq!(applied, 10, "首轮必须全部 Applied");

        // 重放同一批（模拟断网重启后重复投递）：不得再次成为候选、不得再次 Applied。
        let again = replay.next_replay_batch(10).expect("replay batch 2");
        assert!(again.is_empty(), "已判定过的条目不得再次成为补发候选");
        let key = batch[0].idempotency_key();
        assert_eq!(
            replay.mark_sent_key(&key),
            DedupOutcome::Duplicate {
                reason: DedupReason::AlreadyApplied
            },
            "同一幂等键的第二次判定必须是 Duplicate"
        );
        let stats = replay.stats();
        assert_eq!(stats.applied_rows, 10, "幂等账本只应 Applied 10 次");
        assert_eq!(stats.duplicate_rows, 1);
    }

    /// 接线点 2（QA）：**Ack 丢失**（持久化失败）→ 位点一步不动；重放仍被幂等去重。
    #[test]
    fn lost_ack_keeps_cursor_and_replayed_batch_still_dedups() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = QueueConfig::new(dir.path().join("queue.db"), "gw-ack").expect("cfg");
        let hooks = QueueHooks {
            ack_sink: Some(Arc::new(FailingAckSink)),
            ..QueueHooks::default()
        };
        let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(0));
        let queue = Arc::new(OfflineQueue::open_with_hooks(cfg, clock, hooks).expect("open queue"));
        for i in 0..3 {
            queue
                .enqueue(format!("d{i}").into_bytes())
                .expect("enqueue");
        }
        let audit = Arc::new(AuditLog::default());
        let replay = NorthReplay::new("gw-ack", Arc::clone(&queue), audit);

        let batch = replay.next_replay_batch(3).expect("batch");
        assert_eq!(batch.len(), 3);
        for b in &batch {
            assert!(replay.mark_sent(b.seq).is_applied());
        }
        let last = batch.last().expect("non-empty batch").seq;

        let before = replay.high_water_mark();
        assert!(replay.ack(last).is_err(), "Ack 落盘失败必须返回错误");
        assert_eq!(replay.high_water_mark(), before, "Ack 失败位点不得推进");
        assert_eq!(queue.ack_seq(), before, "队列位点同样不得推进");

        // 位点未推进 → 该批仍是补发候选；但幂等账本记着 → 重放仍去重。
        let again = replay.next_replay_batch(3).expect("batch 2");
        assert!(again.is_empty(), "幂等窗口内的批次不得重复补发");
        for b in &batch {
            assert!(
                matches!(replay.mark_sent(b.seq), DedupOutcome::Duplicate { .. }),
                "重发必须去重"
            );
        }
        assert_eq!(replay.stats().applied_rows, 3);
    }

    /// 接线点 3（QA）：审计环有界 → `dropped()` 可观测「审计消费变慢」；
    /// `drain()` 有真实调用点，事件真的上报到出口。
    #[test]
    fn audit_pump_drains_bounded_buffer_and_reports_drops() {
        let log = Arc::new(AuditLog::new(4));
        for i in 0..10i64 {
            log.record(BackpressureAudit::HighWaterCleared {
                rows: u64::try_from(i).unwrap_or(0),
                bytes: 0,
                ts_ns: i,
            });
        }
        assert_eq!(log.dropped(), 6, "有界环必须淘汰最旧并计数");

        let sink = Arc::new(RecordingAuditSink::default());
        let pump = AuditPump::new(Arc::clone(&log), sink.clone());
        assert_eq!(pump.pump(), 4, "drain 必须取走缓冲内全部 4 条");
        assert_eq!(pump.log().len(), 0, "drain 后缓冲必须清空");
        assert_eq!(sink.events().len(), 4, "审计必须真的上报到出口");
        assert_eq!(pump.pump(), 0, "空环再泵为 0");
        assert_eq!(pump.dropped(), 6);
        assert_eq!(pump.emitted(), 10);
    }

    /// 接线点 2（消费侧，QA）：只有 `Applied` 才入库；重放 / 跨网关 / 非法键一律跳过。
    #[test]
    fn ingest_gate_admits_only_new_keys() {
        let audit = Arc::new(AuditLog::default());
        let gate = IngestGate::new("gw-in", audit);

        assert!(gate.admit("gw-in:1").is_applied());
        assert_eq!(
            gate.admit("gw-in:1"),
            DedupOutcome::Duplicate {
                reason: DedupReason::AlreadyApplied
            },
            "同一键重复投递必须跳过（不入库）"
        );
        assert!(matches!(
            gate.admit("gw-other:2"),
            DedupOutcome::Duplicate {
                reason: DedupReason::OtherGateway { .. }
            }
        ));
        assert!(matches!(
            gate.admit("not-a-key"),
            DedupOutcome::Duplicate {
                reason: DedupReason::MalformedKey { .. }
            }
        ));

        gate.confirm_up_to(1);
        assert_eq!(gate.high_water_mark(), 1);
        assert!(matches!(
            gate.admit("gw-in:1"),
            DedupOutcome::Duplicate {
                reason: DedupReason::BelowHighWater { high_water: 1 }
            }
        ));
        assert!(gate.admit("gw-in:2").is_applied());
        assert_eq!(gate.stats().applied_rows, 2);
    }

    /// 接线点 2（真实链路，QA）：发布成功后**才**推进位点；位点推进后再次补发不得重复。
    #[tokio::test]
    async fn replay_pump_publishes_then_advances_pointer() {
        let broker = spawn_mock_broker(BrokerOptions::default()).await;
        let (_dir, queue) = temp_queue("gw-rp");
        for i in 0..3 {
            queue
                .enqueue(format!("r{i}").into_bytes())
                .expect("enqueue");
        }
        let (mut client, outlet, _sink) =
            client_with_outlet(broker.port, "iot-daq-rp1", "gw-rp", &queue);
        client.poll_event().await.expect("connect");

        let report = client.pump_replay("replay").await.expect("replay pump");
        assert_eq!(report.candidates, 3);
        assert_eq!(report.sent, 3);
        assert_eq!(report.deduped, 0);
        assert_eq!(report.failed, 0);
        assert_eq!(report.ack_errors, 0);
        assert!(report.acked, "发布成功后必须推进位点");
        assert_eq!(outlet.replay().high_water_mark(), 3);
        assert_eq!(queue.ack_seq(), 3, "Ack 已落盘 → 队列位点推进");

        // 位点已推进 → 再次补发无候选，网络上不出现重复数据。
        let again = client.pump_replay("replay").await.expect("replay pump 2");
        assert_eq!(again.candidates, 0);

        // 推进事件循环把已提交报文真正写到线路（`publish` 只入请求通道）。
        for _ in 0..32 {
            if broker.publishes().len() >= 3 {
                break;
            }
            let _ = tokio::time::timeout(Duration::from_secs(3), client.poll_event()).await;
        }
        let observed = broker.publishes();
        assert_eq!(observed.len(), 3, "不得重复补发：{observed:?}");
        let mut seen: Vec<Vec<u8>> = observed.iter().map(|p| p.payload.clone()).collect();
        seen.sort();
        assert_eq!(seen, vec![b"r0".to_vec(), b"r1".to_vec(), b"r2".to_vec()]);
    }
}
