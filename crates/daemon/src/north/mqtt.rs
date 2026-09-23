//! 北向 MQTT 客户端（计划 task 19）：rumqttc（纯 Rust + rustls）单客户端 + 多路连接池。
//!
//! ## 职责边界
//! - **做**：单路 [`MqttClient`] 的连接配置 / 选项构建 / 发布订阅 / 事件循环推进 /
//!   自动重连与会话恢复 / 退避策略；多路连接按名管理的 [`MqttConnectionPool`]；
//!   **每路连接独立声明 `encoding`**（[`Encoding::Protobuf`] 默认 / [`Encoding::Json`]）。
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

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rumqttc::{
    AsyncClient, ConnectReturnCode, ConnectionError, Event, EventLoop, MqttOptions, Packet, QoS,
    Transport,
};
use rustls::crypto::CryptoProvider;

use crate::driver::Reconnector;
use crate::error::{DaemonError, DaemonResult};
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
/// - `ca_cert_path`：服务端 CA（必填，用于校验 broker 证书）；
/// - `client_cert_path` + `client_key_path`：成对出现即启用 mTLS。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TlsConfig {
    /// CA 证书 PEM 路径（必填）。
    pub ca_cert_path: Option<PathBuf>,
    /// 客户端证书 PEM 路径（mTLS；与 `client_key_path` 成对）。
    pub client_cert_path: Option<PathBuf>,
    /// 客户端私钥 PEM 路径（mTLS；与 `client_cert_path` 成对）。
    pub client_key_path: Option<PathBuf>,
    /// ALPN 协议列表（如 `b"mqtt"`；空 = 不协商）。
    pub alpn: Vec<Vec<u8>>,
}

impl TlsConfig {
    /// 仅服务端校验（无 mTLS）。
    pub fn ca_only(ca_cert_path: impl Into<PathBuf>) -> Self {
        Self {
            ca_cert_path: Some(ca_cert_path.into()),
            client_cert_path: None,
            client_key_path: None,
            alpn: Vec::new(),
        }
    }

    /// mTLS（服务端校验 + 客户端证书）。
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

    /// 配置自检：CA 必填、mTLS 证书与私钥必须成对。
    ///
    /// # Errors
    /// 非法组合返回 [`DaemonError::ConfigError`]（2000）。
    pub fn validate(&self) -> DaemonResult<()> {
        if self.ca_cert_path.is_none() {
            return Err(DaemonError::ConfigError(
                "outlet tls: ca_cert_path is required when tls is enabled".to_string(),
            ));
        }
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

    /// 读取 PEM 并构造 rumqttc [`Transport`]（rustls）。
    ///
    /// 只做**配置→传输**的映射（PEM 内容在此读入内存），真正的 rustls 握手配置由
    /// rumqttc 在建连阶段惰性构建；本方法会先确保进程级 `CryptoProvider` 就位
    /// （见 [`ensure_rustls_provider`]），因此**宿主二进制无需额外调用
    /// `install_default()`**。
    ///
    /// # Errors
    /// 文件读取/内容非法 → [`DaemonError::SecurityError`]（7000）；
    /// 配置非法 → [`DaemonError::ConfigError`]（2000）。
    pub fn to_transport(&self) -> DaemonResult<Transport> {
        self.validate()?;
        ensure_rustls_provider()?;

        let ca = read_pem(
            self.ca_cert_path
                .as_ref()
                .ok_or_else(|| DaemonError::ConfigError("outlet tls: missing ca".to_string()))?,
            "ca_cert_path",
        )?;

        let client_auth = match (&self.client_cert_path, &self.client_key_path) {
            (Some(cert), Some(key)) => Some((
                read_pem(cert, "client_cert_path")?,
                read_pem(key, "client_key_path")?,
            )),
            _ => None,
        };

        let alpn = if self.alpn.is_empty() {
            None
        } else {
            Some(self.alpn.clone())
        };

        Ok(Transport::tls(ca, client_auth, alpn))
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
#[derive(Debug, Clone)]
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

        let mut options = MqttOptions::new(&self.client_id, &self.broker, self.port);
        options.set_keep_alive(self.keep_alive);
        options.set_clean_session(self.clean_session);
        options.set_request_channel_capacity(self.request_channel_capacity);
        options.set_inflight(self.inflight);

        if let (Some(username), Some(password)) = (&self.username, &self.password) {
            options.set_credentials(username, password);
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

    /// 会话恢复后待重发的报文数（`EventLoop::pending` 长度）。
    pub fn pending_requests(&self) -> usize {
        self.eventloop.pending.len()
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
            Ok(event) => Ok(event),
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

impl fmt::Debug for MqttClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MqttClient")
            .field("endpoint", &self.endpoint)
            .field("session_present", &self.session_present)
            .field("connected", &self.connected)
            .field("connect_count", &self.connect_count)
            .field("retry_after", &self.retry_after)
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

    use crate::error::{ERR_CONFIG, ERR_MQTT, ERR_NETWORK, ERR_SECURITY};

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

    /// QA: TLS 缺 CA / mTLS 只有半套 → ConfigError（证书域自检在构建期完成）。
    #[test]
    fn incomplete_tls_config_is_rejected() {
        let no_ca = TlsConfig::default();
        assert_eq!(
            no_ca.validate().expect_err("missing ca").error_code(),
            ERR_CONFIG
        );

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

    /// QA: TLS 证书路径 → rumqttc `Transport::Tls`；缺失文件 → SecurityError（7000）。
    #[test]
    fn tls_config_maps_to_rustls_transport() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ca = dir.path().join("ca.pem");
        std::fs::write(&ca, b"-----BEGIN CERTIFICATE-----\ntest-only\n").expect("write ca");

        let config = TlsConfig::ca_only(&ca);
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
        let dir = tempfile::tempdir().expect("tempdir");
        let ca = dir.path().join("ca.pem");
        std::fs::write(&ca, b"-----BEGIN CERTIFICATE-----\ntest-only\n").expect("write ca");
        match TlsConfig::ca_only(&ca).to_transport() {
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
}
